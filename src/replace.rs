use crate::{derivation, editor, log, rebuild, store};
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct ReplaceResult {
    pub new_root: PathBuf,
    /// (original path, grafted/rebuilt path) for every path actually
    /// rewritten, dependencies before dependents.
    pub grafted: Vec<(PathBuf, PathBuf)>,
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

    if opts.dry_run {
        let mut memo = HashMap::new();
        for p in &closure_paths {
            if !would_change(p, ctx.explicit, ctx.cutoffs, &mut memo)? {
                continue;
            }
            // No graft/rebuild strategy applies to an explicit target.
            if let Some(new) = ctx.explicit.get(p) {
                eprintln!("[dry-run] {} is an explicit replacement target -> {}", p.display(), new.display());
                continue;
            }
            let verb = if ctx.full_rebuild || ctx.force_rebuild.contains(p) { "rebuild" } else { "graft" };
            let verb = if ctx.force_graft.contains(p) { "graft" } else { verb };
            if verb == "rebuild" {
                let changed_refs = changed_direct_refs(p, ctx.explicit, ctx.cutoffs, &mut memo)?;
                report_rebuild_feasibility(p, &changed_refs)?;
            } else {
                eprintln!("[dry-run] would {verb} {}", p.display());
            }
        }
        return Ok(ReplaceResult {
            new_root: closure_root.to_path_buf(),
            grafted: Vec::new(),
        });
    }

    let mut memo: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut grafted = Vec::new();
    for p in &closure_paths {
        rewrite_one(p, &ctx, &mut memo, &mut grafted)
            .with_context(|| format!("while grafting {}", p.display()))?;
    }
    let new_root = memo
        .get(closure_root)
        .cloned()
        .with_context(|| format!("closure root {} missing from rewrite map", closure_root.display()))?;
    Ok(ReplaceResult { new_root, grafted })
}

/// The parts of a `replace` invocation that stay constant across the whole
/// recursive walk, bundled so `rewrite_one` doesn't need one parameter per
/// option.
struct Ctx<'a> {
    explicit: &'a HashMap<PathBuf, PathBuf>,
    cutoffs: &'a HashSet<PathBuf>,
    force_rebuild: &'a HashSet<PathBuf>,
    force_graft: &'a HashSet<PathBuf>,
    nix_args: &'a [String],
    full_rebuild: bool,
}

/// Whether `path`, or anything it transitively references, is an explicit
/// replacement target (`false` for a `cutoffs` member regardless — that's
/// what a cutoff means). Mirrors `rewrite_one`'s real precedence (explicit >
/// cutoff > default); takes `explicit`/`cutoffs` directly rather than a
/// whole `Ctx` since `--interactive` needs this result before
/// `force_rebuild`/`force_graft` are finalized.
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

/// Memoized post-order rewrite: dependencies are rewritten before the paths
/// that reference them, so grafting a path can always assume its changed
/// references already point at real, final store paths.
fn rewrite_one(
    path: &Path,
    ctx: &Ctx,
    memo: &mut HashMap<PathBuf, PathBuf>,
    grafted: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<PathBuf> {
    if let Some(p) = memo.get(path) {
        return Ok(p.clone());
    }
    // Explicit replacements are trusted as-is, not recursed into further.
    if let Some(new) = ctx.explicit.get(path) {
        log::v(format!("{} is an explicit replacement target -> {}", path.display(), new.display()));
        memo.insert(path.to_path_buf(), new.clone());
        return Ok(new.clone());
    }
    // Left as-is unconditionally — not even its own references are checked.
    if ctx.cutoffs.contains(path) {
        log::v(format!("{}: cutoff, left as-is (not checking its references)", path.display()));
        memo.insert(path.to_path_buf(), path.to_path_buf());
        return Ok(path.to_path_buf());
    }
    // Every direct reference, rewritten (or mapped to itself if unaffected).
    let mut all_refs = Vec::new();
    let mut any_changed = false;
    for r in store::references(path)? {
        let rewritten = rewrite_one(&r, ctx, memo, grafted)?;
        any_changed |= rewritten != r;
        all_refs.push((r, rewritten));
    }
    let result = if !any_changed {
        log::v(format!("{}: no changed references, left as-is", path.display()));
        path.to_path_buf()
    } else {
        let use_rebuild = (ctx.full_rebuild || ctx.force_rebuild.contains(path)) && !ctx.force_graft.contains(path);
        let changed_count = all_refs.iter().filter(|(o, n)| o != n).count();
        log::v(format!(
            "{}: {changed_count} changed reference(s), {} -> building",
            path.display(),
            if use_rebuild { "rebuilding" } else { "grafting" }
        ));
        let new_path = if use_rebuild {
            rebuild::rebuild_path(path, &all_refs, ctx.nix_args)?
        } else {
            graft_path(path, &all_refs, ctx.nix_args)?
        };
        log::v(format!("{} -> {}", path.display(), new_path.display()));
        grafted.push((path.to_path_buf(), new_path.clone()));
        new_path
    };
    memo.insert(path.to_path_buf(), result.clone());
    Ok(result)
}

/// Graft `path` by building a tiny synthetic derivation whose builder does
/// `nix-store --dump | sed | nix-store --restore` (the same technique Guix
/// and nixpkgs's `replaceDependencies` use), then realising it normally.
///
/// Goes through a real derivation build rather than `nix store add`
/// deliberately: that path never registers references (confirmed
/// empirically), leaving the grafted path unprotected from GC and invisible
/// to further closure walks. Registering references directly requires
/// `nix-store --register-validity`, which needs trusted-user/root
/// privileges a normal build doesn't. See DESIGN.md §4 for the full story.
fn graft_path(path: &Path, all_refs: &[(PathBuf, PathBuf)], nix_args: &[String]) -> Result<PathBuf> {
    let changed: Vec<&(PathBuf, PathBuf)> = all_refs.iter().filter(|(o, n)| o != n).collect();

    let mut sed_expr = String::new();
    for (old, new) in &changed {
        let old_b = store::basename(old)?;
        let new_b = store::basename(new)?;
        if old_b.len() != new_b.len() {
            // Should be unreachable: every `new` reaching here either came
            // from a user-validated top-level pair, or from a prior
            // graft_path call, which always preserves the original name.
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

    derivation::build_and_realise(
        derivation::DerivationSpec {
            name,
            builder: bash,
            args: vec!["-c".to_string(), script],
            input_srcs,
        },
        nix_args,
    )
}
