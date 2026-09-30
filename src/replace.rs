use crate::{derivation, diff, editor, log, rebuild, report, store};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct ReplaceResult {
    pub new_root: PathBuf,
}

/// Per-category counts across the whole closure, printed as a Guix-style
/// closing summary — computed and reported entirely inside `replace()`
/// itself, matching how `--dry-run`'s per-path reporting already works;
/// there's no external consumer for the structured counts, so they don't
/// leave this module.
#[derive(Default, Clone, Copy)]
struct Tally {
    grafted: usize,
    rebuilt: usize,
    cutoff: usize,
    unchanged: usize,
    explicit: usize,
}

impl Tally {
    fn total(&self) -> usize {
        self.grafted + self.rebuilt + self.cutoff + self.unchanged + self.explicit
    }

    /// "12 grafted, 2 rebuilt, 43 unchanged (57 total)" — zero-count
    /// categories omitted, matching how Guix reports a plan: state what's
    /// actually happening, not every category whether or not it applies.
    fn summarize(&self) -> String {
        let parts: Vec<String> = [
            (self.grafted, "grafted"),
            (self.rebuilt, "rebuilt"),
            (self.explicit, "explicit replacement"),
            (self.cutoff, "cutoff"),
            (self.unchanged, "unchanged"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, label)| format!("{n} {label}"))
        .collect();
        if parts.is_empty() {
            "nothing to do".to_string()
        } else {
            format!("{} ({} total)", parts.join(", "), self.total())
        }
    }
}

/// Strategy knobs, shared verbatim by `replace` and every `edit_*`
/// subcommand — see `main.rs`'s `StrategyArgs` for the CLI-facing docs on
/// each field (kept there rather than duplicated here).
pub struct ReplaceOptions<'a> {
    pub dry_run: bool,
    pub nix_args: &'a [String],
    pub full_rebuild: bool,
    pub cutoffs: &'a [PathBuf],
    pub force_rebuild: &'a [PathBuf],
    pub force_graft: &'a [PathBuf],
    pub interactive: bool,
    /// Write an HTML report (dependency graph + details table) here.
    /// Works under `--dry-run` too — anything not yet built just shows as
    /// "(pending)" instead of a concrete new path.
    pub report: Option<&'a Path>,
    /// Embed `nix-diff`/`diffoscope` output per node in the report. Only
    /// meaningful alongside `report` — `replace()` rejects it alone.
    pub report_diff: bool,
}

/// Replace all of `old` with `new` (for each pair in `replacements`), in the
/// context of the closure rooted at `closure_root`. See [`ReplaceOptions`]
/// for the strategy knobs.
///
/// Precedence when a path is affected by more than one of
/// `replacements`/`opts.cutoffs`/`opts.force_rebuild`: an explicit
/// `--replace` target wins over a cutoff, which wins over the default
/// strategy — the same order nixpkgs documents for its own equivalent.
pub fn replace(closure_root: &Path, replacements: &[(PathBuf, PathBuf)], opts: &ReplaceOptions) -> Result<ReplaceResult> {
    if opts.report_diff && opts.report.is_none() {
        bail!("--report-diff has no effect without --report <dir>");
    }
    // Syntactic checks first, before requiring anything to exist on disk.
    store::require_output_path(closure_root)?;
    for (old, new) in replacements {
        store::require_output_path(old)?;
        store::require_output_path(new)?;
    }
    if !opts.full_rebuild {
        for (old, new) in replacements {
            let ob = store::basename(old)?;
            let nb = store::basename(new)?;
            if ob.len() != nb.len() {
                bail!(
                    "cannot replace {} with {}: basenames differ in length ({} vs {}); \
                     a fixed-width rewrite requires equal-length basenames \
                     (pass --rebuild to do a real rebuild instead, which has no such constraint)",
                    old.display(),
                    new.display(),
                    ob.len(),
                    nb.len()
                );
            }
        }
    }

    // Canonicalize only now, so the checks above can still reject an
    // intentionally-nonexistent bogus pair; the memo table below needs
    // exact PathBuf equality with what `nix path-info` returns.
    let closure_root = store::canonicalize(closure_root)?;
    let closure_root = closure_root.as_path();
    let replacements: Vec<(PathBuf, PathBuf)> = replacements
        .iter()
        .map(|(old, new)| Ok((store::canonicalize(old)?, store::canonicalize(new)?)))
        .collect::<Result<_>>()?;
    let mut cutoffs: HashSet<PathBuf> = opts.cutoffs.iter().map(|p| store::canonicalize(p)).collect::<Result<_>>()?;
    let mut force_rebuild: HashSet<PathBuf> =
        opts.force_rebuild.iter().map(|p| store::canonicalize(p)).collect::<Result<_>>()?;
    let mut force_graft: HashSet<PathBuf> = opts.force_graft.iter().map(|p| store::canonicalize(p)).collect::<Result<_>>()?;

    for (old, new) in &replacements {
        log::v(format!("replacement requested: {} -> {}", old.display(), new.display()));
    }
    for p in &cutoffs {
        log::v(format!("cutoff: {} will never be touched", p.display()));
    }
    for p in &force_rebuild {
        log::v(format!("force-rebuild: {} will use --rebuild strategy if it changes", p.display()));
    }
    for p in &force_graft {
        log::v(format!("force-graft: {} will use graft strategy if it changes", p.display()));
    }
    let explicit: HashMap<PathBuf, PathBuf> = replacements.into_iter().collect();
    let closure_paths = store::closure(closure_root)?;
    log::v(format!(
        "closure of {} has {} path(s)",
        closure_root.display(),
        closure_paths.len()
    ));

    for (old, new) in &explicit {
        if !closure_paths.contains(old) {
            eprintln!(
                "warning: {} is not in the closure of {} — this --replace will have no effect \
                 (nothing here can ever encounter it to substitute {})",
                old.display(),
                closure_root.display(),
                new.display()
            );
        }
    }

    if opts.interactive {
        interactive_select(&closure_paths, &explicit, opts.full_rebuild, &mut cutoffs, &mut force_rebuild, &mut force_graft)?;
    }

    let ctx = Ctx {
        explicit: &explicit,
        cutoffs: &cutoffs,
        force_rebuild: &force_rebuild,
        force_graft: &force_graft,
        nix_args: opts.nix_args,
        full_rebuild: opts.full_rebuild,
    };

    let mut nodes: HashMap<PathBuf, Node> = HashMap::new();
    for p in &closure_paths {
        classify(p, &ctx, &mut nodes)?;
    }
    let tally = tally_of(&nodes);

    if opts.dry_run {
        for p in &closure_paths {
            match &nodes[p].category {
                Category::Explicit(new) => {
                    eprintln!("[dry-run] {} is an explicit replacement target -> {}", p.display(), new.display());
                }
                Category::Cutoff | Category::Unchanged => {}
                Category::NeedsGraft => eprintln!("[dry-run] would graft {}", p.display()),
                Category::NeedsRebuild => {
                    let changed_refs = changed_refs_of(p, &nodes)?;
                    report_rebuild_feasibility(p, &changed_refs)?;
                }
            }
        }
        eprintln!("[dry-run] {}", tally.summarize());
        write_report_if_requested(opts, closure_root, closure_root, &nodes, None, &tally)?;
        return Ok(ReplaceResult { new_root: closure_root.to_path_buf() });
    }

    // Seed every path that needs no build; bucket the rest by dependency
    // level so each level's builds can run in parallel — see `classify`'s
    // doc comment for why paths in the same level are provably independent.
    let mut resolved: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut by_level: BTreeMap<usize, Vec<PathBuf>> = BTreeMap::new();
    for (path, node) in &nodes {
        match &node.category {
            Category::Explicit(new) => {
                resolved.insert(path.clone(), new.clone());
            }
            Category::Cutoff | Category::Unchanged => {
                resolved.insert(path.clone(), path.clone());
            }
            Category::NeedsGraft | Category::NeedsRebuild => {
                by_level.entry(node.level).or_default().push(path.clone());
            }
        }
    }

    for (level, paths) in &by_level {
        // Phase 1: construct every recipe in parallel — cheap (`nix
        // derivation add`, no building), but still worth spreading across
        // threads since each one is a handful of subprocess round-trips.
        log::v(format!("level {level}: constructing {} recipe(s) in parallel", paths.len()));
        let recipes: Vec<(PathBuf, Result<(PathBuf, String)>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = paths
                .iter()
                .map(|path| {
                    let path = path.clone();
                    let use_rebuild = matches!(nodes[&path].category, Category::NeedsRebuild);
                    let resolved = &resolved;
                    scope.spawn(move || {
                        let outcome = build_recipe(&path, use_rebuild, resolved);
                        (path, outcome)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("recipe-construction worker thread panicked")).collect()
        });

        // Phase 2: one `nix build` call for the whole level. Bails on the
        // first failed recipe (in the same deterministic `paths` order the
        // old one-path-at-a-time walk reported errors in) before spending
        // anything on a build we already know won't fully succeed.
        let mut targets = Vec::with_capacity(recipes.len());
        let mut recipe_of: HashMap<PathBuf, (PathBuf, String)> = HashMap::new();
        for (path, outcome) in recipes {
            let verb = if matches!(nodes[&path].category, Category::NeedsRebuild) { "rebuilding" } else { "grafting" };
            let recipe = outcome.with_context(|| format!("while {verb} {}", path.display()))?;
            targets.push(recipe.clone());
            recipe_of.insert(path, recipe);
        }
        log::v(format!("level {level}: building {} target(s) in one nix build call", targets.len()));
        let built = derivation::build_many(&targets, ctx.nix_args)?;

        // Phase 3: map each path's recipe back to its built output.
        for (path, (drv, output_name)) in recipe_of {
            let new_path = built.get(&(drv.clone(), output_name.clone())).cloned().with_context(|| {
                format!("nix build did not report an output for {}'s recipe ({}^{output_name})", path.display(), drv.display())
            })?;
            log::v(format!("{} -> {}", path.display(), new_path.display()));
            eprintln!(
                "{} {} -> {}",
                if matches!(nodes[&path].category, Category::NeedsRebuild) { "rebuilt" } else { "grafted" },
                path.display(),
                new_path.display()
            );
            resolved.insert(path, new_path);
        }
    }

    eprintln!("{}", tally.summarize());
    let new_root = resolved
        .get(closure_root)
        .cloned()
        .with_context(|| format!("closure root {} missing from resolved map", closure_root.display()))?;
    write_report_if_requested(opts, closure_root, &new_root, &nodes, Some(&resolved), &tally)?;
    Ok(ReplaceResult { new_root })
}

/// `--report <dir>`: an HTML dependency graph + details table for
/// everything that changed or caused a change — cutoff/unchanged paths are
/// deliberately excluded, since a large closure's "nothing happened here"
/// majority is noise, not signal, for a report meant to answer "what did
/// this graft actually do."
fn write_report_if_requested(
    opts: &ReplaceOptions,
    closure_root: &Path,
    new_root: &Path,
    nodes: &HashMap<PathBuf, Node>,
    resolved: Option<&HashMap<PathBuf, PathBuf>>,
    tally: &Tally,
) -> Result<()> {
    let Some(dir) = opts.report else { return Ok(()) };
    // Created here, before any diffoscope call — diffoscope doesn't create
    // its own output directory and fails with a raw traceback if it's missing.
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create report directory {}", dir.display()))?;
    let report_nodes = build_report_nodes(dir, opts.report_diff, nodes, resolved)?;
    let index = report::write(dir, closure_root, new_root, opts.dry_run, &tally.summarize(), &report_nodes)?;
    eprintln!("wrote report to {}", index.display());
    Ok(())
}

fn build_report_nodes(
    dir: &Path,
    report_diff: bool,
    nodes: &HashMap<PathBuf, Node>,
    resolved: Option<&HashMap<PathBuf, PathBuf>>,
) -> Result<Vec<report::ReportNode>> {
    // `Cutoff` is a deliberate decision worth showing (part of the graft's
    // story: "propagation stopped here, on purpose") — only `Unchanged`
    // ("nothing happened here") is genuine noise.
    let included: HashSet<&PathBuf> = nodes.iter().filter(|(_, n)| !matches!(n.category, Category::Unchanged)).map(|(p, _)| p).collect();
    let mut out = Vec::new();
    for (path, node) in nodes {
        if !included.contains(path) {
            continue;
        }
        let (label, color, new_path, diffable) = match &node.category {
            Category::Explicit(new) => ("explicit replacement", "#2563eb", Some(new.clone()), true),
            Category::NeedsGraft => ("grafted", "#16a34a", resolved.and_then(|r| r.get(path).cloned()), true),
            Category::NeedsRebuild => ("rebuilt", "#ea580c", resolved.and_then(|r| r.get(path).cloned()), true),
            // Resolves to itself by definition — nothing to diff against.
            Category::Cutoff => ("cutoff", "#6b7280", Some(path.clone()), false),
            Category::Unchanged => unreachable!("filtered out above"),
        };
        let depends_on = store::references(path)?.into_iter().filter(|r| included.contains(r)).collect();

        let (nix_diff, diffoscope_html) = if report_diff && diffable {
            diffs_for(dir, path, new_path.as_deref(), matches!(node.category, Category::NeedsGraft))
        } else {
            (None, None)
        };

        out.push(report::ReportNode { path: path.clone(), label, color, new_path, depends_on, nix_diff, diffoscope_html });
    }
    Ok(out)
}

/// Best-effort per-node diffs for `--report-diff`: never fails the whole
/// report over one node's diff tooling — a missing `nix-diff`/`diffoscope`,
/// or a path with no known deriver, just means that node's diff is omitted
/// (logged under `-v`), not a hard error for an otherwise-successful graft.
fn diffs_for(dir: &Path, path: &Path, new_path: Option<&Path>, is_graft: bool) -> (Option<String>, Option<String>) {
    let Some(new_path) = new_path else { return (None, None) };

    // Grafting never changes the derivation — nothing for nix-diff to show.
    let nix_diff = if is_graft {
        None
    } else {
        match (derivation::deriver_of(path), derivation::deriver_of(new_path)) {
            (Ok(old_drv), Ok(new_drv)) => match diff::nix_diff(&old_drv, &new_drv) {
                Ok(text) => Some(text),
                Err(e) => {
                    log::v(format!("nix-diff for {} skipped: {e}", path.display()));
                    None
                }
            },
            _ => {
                log::v(format!("nix-diff for {} skipped: no known deriver on one or both sides", path.display()));
                None
            }
        }
    };

    let file_name = format!("diffoscope-{}.html", store::basename(path).unwrap_or_else(|_| "unknown".to_string()));
    let out_html = dir.join(&file_name);
    let diffoscope_html = match diff::diffoscope_html(path, new_path, &out_html) {
        Ok(()) => Some(file_name),
        Err(e) => {
            log::v(format!("diffoscope for {} skipped: {e}", path.display()));
            None
        }
    };

    (nix_diff, diffoscope_html)
}

/// The parts of a `replace` invocation that stay constant across the whole
/// walk, bundled so `classify`/`build_recipe` don't need one parameter per
/// option. Shared read-only across worker threads during the parallel
/// recipe-construction phase — every field is `Sync` (no interior
/// mutability), so no locking is needed.
struct Ctx<'a> {
    explicit: &'a HashMap<PathBuf, PathBuf>,
    cutoffs: &'a HashSet<PathBuf>,
    force_rebuild: &'a HashSet<PathBuf>,
    force_graft: &'a HashSet<PathBuf>,
    nix_args: &'a [String],
    full_rebuild: bool,
}

#[derive(Clone)]
enum Category {
    /// A literal `--replace` target: resolves to this path directly, never
    /// recursed into further.
    Explicit(PathBuf),
    /// Never touched, no matter what changed beneath it.
    Cutoff,
    /// No direct or transitive reference changed; resolves to itself.
    Unchanged,
    /// Needs a blind NAR byte-substitution.
    NeedsGraft,
    /// Needs a real dependency substitution + sandboxed rebuild.
    NeedsRebuild,
}

#[derive(Clone)]
struct Node {
    category: Category,
    /// Meaningful only for `NeedsGraft`/`NeedsRebuild`: `1 +` the highest
    /// level among direct references that themselves need a build. Two
    /// nodes at the same level are provably independent — neither can be a
    /// (transitive) reference of the other, since that would force its
    /// level strictly higher — so a whole level's builds can run in
    /// parallel with no risk of one needing the other's not-yet-built result.
    level: usize,
}

/// Memoized classification: what `path` resolves to (via [`Category`]) and,
/// for anything needing a build, its dependency level for scheduling.
/// Precedence mirrors [`replace`]'s doc comment: explicit > cutoff >
/// default. Recursion stops at a cutoff without even inspecting its own
/// references, matching what a cutoff means.
fn classify(path: &Path, ctx: &Ctx, memo: &mut HashMap<PathBuf, Node>) -> Result<Node> {
    if let Some(n) = memo.get(path) {
        return Ok(n.clone());
    }
    let node = if let Some(new) = ctx.explicit.get(path) {
        log::v(format!("{} is an explicit replacement target -> {}", path.display(), new.display()));
        Node { category: Category::Explicit(new.clone()), level: 0 }
    } else if ctx.cutoffs.contains(path) {
        log::v(format!("{}: cutoff, left as-is (not checking its references)", path.display()));
        Node { category: Category::Cutoff, level: 0 }
    } else {
        let mut max_dep_level = 0;
        let mut any_changed = false;
        for r in store::references(path)? {
            let rn = classify(&r, ctx, memo)?;
            match &rn.category {
                Category::Explicit(_) => any_changed = true,
                Category::NeedsGraft | Category::NeedsRebuild => {
                    any_changed = true;
                    max_dep_level = max_dep_level.max(rn.level);
                }
                Category::Cutoff | Category::Unchanged => {}
            }
        }
        if any_changed {
            let use_rebuild = (ctx.full_rebuild || ctx.force_rebuild.contains(path)) && !ctx.force_graft.contains(path);
            Node {
                category: if use_rebuild { Category::NeedsRebuild } else { Category::NeedsGraft },
                level: max_dep_level + 1,
            }
        } else {
            log::v(format!("{}: no changed references, left as-is", path.display()));
            Node { category: Category::Unchanged, level: 0 }
        }
    };
    memo.insert(path.to_path_buf(), node.clone());
    Ok(node)
}

fn tally_of(nodes: &HashMap<PathBuf, Node>) -> Tally {
    let mut t = Tally::default();
    for node in nodes.values() {
        match node.category {
            Category::Explicit(_) => t.explicit += 1,
            Category::Cutoff => t.cutoff += 1,
            Category::Unchanged => t.unchanged += 1,
            Category::NeedsGraft => t.grafted += 1,
            Category::NeedsRebuild => t.rebuilt += 1,
        }
    }
    t
}

/// `path`'s direct references already classified as themselves changing —
/// the identities `rebuild::unlocatable_dependencies` needs to check
/// feasibility against, for `--dry-run`'s reporting.
fn changed_refs_of(path: &Path, nodes: &HashMap<PathBuf, Node>) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for r in store::references(path)? {
        if let Some(n) = nodes.get(&r) {
            if !matches!(n.category, Category::Cutoff | Category::Unchanged) {
                out.push(r);
            }
        }
    }
    Ok(out)
}

/// Constructs `path`'s graft/rebuild recipe (a `.drv` plus output name, not
/// yet built) for the parallel construction phase: looks up each direct
/// reference's already-resolved value (guaranteed present — by
/// construction, every direct reference is either resolved with no build at
/// all, or was classified at a strictly lower level, already merged into
/// `resolved` before this level started) and dispatches to
/// `graft_recipe`/`rebuild::rebuild_recipe`. Building the recipe is a
/// separate step (`derivation::build_many`, batched across the whole level).
fn build_recipe(path: &Path, use_rebuild: bool, resolved: &HashMap<PathBuf, PathBuf>) -> Result<(PathBuf, String)> {
    let all_refs: Vec<(PathBuf, PathBuf)> = store::references(path)?
        .into_iter()
        .map(|r| {
            let new_r = resolved.get(&r).cloned().unwrap_or_else(|| r.clone());
            (r, new_r)
        })
        .collect();
    if use_rebuild {
        rebuild::rebuild_recipe(path, &all_refs)
    } else {
        graft_recipe(path, &all_refs)
    }
}

/// Whether `path`, or anything it transitively references, is an explicit
/// replacement target (`false` for a `cutoffs` member regardless — that's
/// what a cutoff means). Used only by `--interactive`'s discovery step,
/// which runs *before* `cutoffs`/`force_rebuild`/`force_graft` are
/// finalized — `classify` can't be used there since it depends on those
/// being final.
fn would_change(
    path: &Path,
    explicit: &HashMap<PathBuf, PathBuf>,
    cutoffs: &HashSet<PathBuf>,
    memo: &mut HashMap<PathBuf, bool>,
) -> Result<bool> {
    if let Some(v) = memo.get(path) {
        return Ok(*v);
    }
    if explicit.contains_key(path) {
        memo.insert(path.to_path_buf(), true);
        return Ok(true);
    }
    if cutoffs.contains(path) {
        memo.insert(path.to_path_buf(), false);
        return Ok(false);
    }
    let mut changed = false;
    for r in store::references(path)? {
        if would_change(&r, explicit, cutoffs, memo)? {
            changed = true;
        }
    }
    memo.insert(path.to_path_buf(), changed);
    Ok(changed)
}

/// `path`'s direct references that would themselves change — the identities
/// `rebuild::unlocatable_dependencies` needs to check feasibility against.
/// Same caveat as [`would_change`]: pre-finalization use only.
fn changed_direct_refs(
    path: &Path,
    explicit: &HashMap<PathBuf, PathBuf>,
    cutoffs: &HashSet<PathBuf>,
    memo: &mut HashMap<PathBuf, bool>,
) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for r in store::references(path)? {
        if would_change(&r, explicit, cutoffs, memo)? {
            out.push(r);
        }
    }
    Ok(out)
}

/// `--dry-run` reporting for a path that would use the rebuild strategy:
/// checks feasibility via `rebuild::unlocatable_dependencies` first, so this
/// reports a fact about what a real run would do rather than a guess.
fn report_rebuild_feasibility(path: &Path, changed_refs: &[PathBuf]) -> Result<()> {
    match rebuild::unlocatable_dependencies(path, changed_refs)? {
        None => {
            eprintln!(
                "[dry-run] would attempt --rebuild for {} but it has no deriver at all — \
                 this WILL fail; pass --force-graft {} or --cutoff {} instead",
                path.display(),
                path.display(),
                path.display()
            );
        }
        Some(missing) if !missing.is_empty() => {
            let names = missing.iter().map(|m| m.display().to_string()).collect::<Vec<_>>().join(", ");
            eprintln!(
                "[dry-run] would attempt --rebuild for {} but {names} {} not declared in its own \
                 .drv (only reachable via the runtime reference graph) — this WILL fail; pass \
                 --force-graft {} or --cutoff {} instead",
                path.display(),
                if missing.len() == 1 { "is" } else { "are" },
                path.display(),
                path.display()
            );
        }
        _ => eprintln!("[dry-run] would rebuild {}", path.display()),
    }
    Ok(())
}

/// `--interactive`: discover every path that would be affected (same
/// mechanism as `--dry-run`), write it as an editable strategy list, open
/// `$EDITOR`, and fold the result back into `cutoffs`/`force_rebuild`/
/// `force_graft`.
fn interactive_select(
    closure_paths: &[PathBuf],
    explicit: &HashMap<PathBuf, PathBuf>,
    full_rebuild: bool,
    cutoffs: &mut HashSet<PathBuf>,
    force_rebuild: &mut HashSet<PathBuf>,
    force_graft: &mut HashSet<PathBuf>,
) -> Result<()> {
    let mut memo = HashMap::new();
    let mut affected: Vec<PathBuf> = Vec::new();
    for p in closure_paths {
        // No strategy applies to an explicit target; listing it would be misleading.
        if explicit.contains_key(p) {
            continue;
        }
        if would_change(p, explicit, cutoffs, &mut memo)? {
            affected.push(p.clone());
        }
    }
    if affected.is_empty() {
        log::v("interactive: nothing would change, skipping the editor");
        return Ok(());
    }

    // Precompute rebuild feasibility once per path (same check `--dry-run`
    // uses), reused below both for the generated default and to reject an
    // explicit `rebuild` choice on a line that's known to fail.
    let mut rebuild_infeasible: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for p in &affected {
        let changed_refs = changed_direct_refs(p, explicit, cutoffs, &mut memo)?;
        if changed_refs.is_empty() {
            continue;
        }
        match rebuild::unlocatable_dependencies(p, &changed_refs)? {
            None => {
                rebuild_infeasible.insert(p.clone(), changed_refs);
            }
            Some(missing) if !missing.is_empty() => {
                rebuild_infeasible.insert(p.clone(), missing);
            }
            _ => {}
        }
    }

    let mut todo = String::new();
    todo.push_str("# graft interactive strategy selection.\n");
    todo.push_str("# Every path below would be affected by this replacement (leaves first).\n");
    todo.push_str("# Change the leading word (or its one-letter shortcut) to pick a strategy:\n");
    todo.push_str("#   graft   / g - blind NAR byte-substitution, no rebuild\n");
    todo.push_str("#   rebuild / r - real dependency substitution + sandboxed rebuild\n");
    todo.push_str("#   cutoff  / c - never touch this path; propagation stops here\n#\n");
    todo.push_str("# Dependency order is fixed by the graph, not by this file — do not reorder\n");
    todo.push_str("# or delete lines; every path listed here must still be present when you save.\n");
    todo.push_str("# A `# rebuild not possible: ...` line can't use `rebuild` — the dependency\n");
    todo.push_str("# isn't declared in that path's own .drv (only reachable via the runtime\n");
    todo.push_str("# reference graph), so grafting is the only option there.\n\n");
    for p in &affected {
        let mut default = if full_rebuild || force_rebuild.contains(p) { "rebuild" } else { "graft" };
        if force_graft.contains(p) {
            default = "graft";
        }
        match rebuild_infeasible.get(p) {
            Some(missing) => {
                let names = missing.iter().map(|m| m.display().to_string()).collect::<Vec<_>>().join(", ");
                todo.push_str(&format!("graft {}  # rebuild not possible: {names} not declared in its own .drv\n", p.display()));
            }
            None => {
                todo.push_str(&format!("{default} {}\n", p.display()));
            }
        }
    }

    let tmp = tempfile::NamedTempFile::new().context("failed to create scratch file for interactive strategy selection")?;
    std::fs::write(tmp.path(), &todo)?;
    log::v(format!("opening $EDITOR on {} ({} affected path(s))", tmp.path().display(), affected.len()));
    editor::edit(tmp.path())?;
    let edited = std::fs::read_to_string(tmp.path())?;

    let mut seen = HashSet::new();
    for line in edited.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Strip a trailing inline `# ...` comment (e.g. the "rebuild not
        // possible" annotation) before splitting into strategy/path.
        let line = line.split('#').next().unwrap_or(line).trim();
        if line.is_empty() {
            continue;
        }
        let (strategy, path_str) = line
            .split_once(char::is_whitespace)
            .with_context(|| format!("malformed line (expected `<strategy> <path>`): {line}"))?;
        let path = store::canonicalize(Path::new(path_str.trim()))?;
        if !affected.contains(&path) {
            bail!("line names a path that wasn't in the original list — lines must not be added: {line}");
        }
        seen.insert(path.clone());
        match strategy {
            "graft" | "g" => {
                force_graft.insert(path);
            }
            "rebuild" | "r" => {
                if let Some(missing) = rebuild_infeasible.get(&path) {
                    let names = missing.iter().map(|m| m.display().to_string()).collect::<Vec<_>>().join(", ");
                    bail!(
                        "cannot select `rebuild` for {}: {names} not declared in its own .drv \
                         (only reachable via the runtime reference graph) — pick `graft` or \
                         `cutoff` instead: {line}",
                        path.display()
                    );
                }
                force_rebuild.insert(path);
            }
            "cutoff" | "c" => {
                cutoffs.insert(path);
            }
            other => bail!("unknown strategy `{other}` (expected graft/g, rebuild/r, or cutoff/c): {line}"),
        }
    }
    if seen.len() != affected.len() {
        bail!("one or more paths from the original list are missing — lines must not be deleted");
    }
    Ok(())
}

/// Graft `path` by constructing a tiny synthetic derivation whose builder
/// does `nix-store --dump | sed | nix-store --restore` (the same technique
/// Guix and nixpkgs's `replaceDependencies` use) — returns the new `.drv`
/// path and its output name (always `"out"`); building it is a separate
/// step left to the caller, so several independent grafts can be batched
/// into one `nix build` call (see `derivation::build_many`).
///
/// Goes through a real derivation build rather than `nix store add`
/// deliberately: that path never registers references (confirmed
/// empirically), leaving the grafted path unprotected from GC and invisible
/// to further closure walks. Registering references directly requires
/// `nix-store --register-validity`, which needs trusted-user/root
/// privileges a normal build doesn't. See DESIGN.md §4 for the full story.
fn graft_recipe(path: &Path, all_refs: &[(PathBuf, PathBuf)]) -> Result<(PathBuf, String)> {
    let changed: Vec<&(PathBuf, PathBuf)> = all_refs.iter().filter(|(o, n)| o != n).collect();

    let mut sed_expr = String::new();
    for (old, new) in &changed {
        let old_b = store::basename(old)?;
        let new_b = store::basename(new)?;
        if old_b.len() != new_b.len() {
            // Should be unreachable: every `new` reaching here either came
            // from a user-validated top-level pair, or from a prior
            // graft_recipe call, which always preserves the original name.
            bail!(
                "internal invariant violated grafting {}: {} vs {} differ in length",
                path.display(),
                old_b,
                new_b
            );
        }
        sed_expr.push_str(&format!("s|{old_b}|{new_b}|g;"));
    }

    let bash = derivation::tool_path("bash")?;
    let sed = derivation::tool_path("sed")?;
    let nix_store = derivation::tool_path("nix-store")?;
    let name = store::store_name(path)?;

    let script = format!(
        "\"{ns}\" --dump \"{p}\" | \"{sed}\" '{expr}' | \"{ns}\" --restore \"$out\"",
        ns = nix_store.display(),
        p = path.display(),
        sed = sed.display(),
        expr = sed_expr,
    );

    // Declare every direct reference (changed or not) as an input, so the
    // daemon's post-build scan registers unmodified ones too, not just the
    // swapped ones. `store_root` because inputSrcs must name whole store
    // items, not files nested inside them.
    let mut input_srcs: Vec<PathBuf> = vec![
        path.to_path_buf(),
        store::store_root(&bash)?,
        store::store_root(&sed)?,
        store::store_root(&nix_store)?,
    ];
    input_srcs.extend(all_refs.iter().map(|(_, new)| new.clone()));
    input_srcs.sort();
    input_srcs.dedup();

    let new_drv = derivation::construct_derivation(derivation::DerivationSpec {
        name,
        builder: bash,
        args: vec!["-c".to_string(), script],
        input_srcs,
    })?;
    Ok((new_drv, "out".to_string()))
}
