mod derivation;
mod diff;
mod edit_drv;
mod edit_file;
mod edit_nix;
mod editor;
mod installable;
mod log;
mod nixos_system;
mod provenance;
mod rebuild;
mod replace;
mod report;
mod store;

use anyhow::{bail, Context, Result};
use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use replace::ReplaceOptions;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Guix-style graft/rewrite prototype for the Nix store.
#[derive(Parser)]
#[command(name = "graft", version)]
struct Cli {
    /// Narrate what graft itself is doing: paths examined, commands run,
    /// decisions made. Separate from `-- <nix args>`, which controls the
    /// verbosity of the underlying `nix build` calls, not graft's own.
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Cmd,
}

/// The common `nix build`/`nix eval` flags, under their real `nix` names
/// and shorts, available directly on every subcommand instead of needing
/// `-- <nix args>` for the ones most people actually reach for. Rendered
/// back into the equivalent `nix` CLI tokens and merged ahead of whatever
/// `-- <nix args>` adds, so both work together.
#[derive(Args)]
struct NixPassthroughArgs {
    /// Print full build logs on standard error.
    #[arg(short = 'L', long)]
    print_build_logs: bool,
    /// Maximum number of build jobs Nix will run in parallel (`auto` for
    /// the number of CPU cores).
    #[arg(short = 'j', long)]
    max_jobs: Option<String>,
    /// Maximum number of CPU cores a single build job can use.
    #[arg(long)]
    cores: Option<String>,
    /// Remote build machines to use, in `nix.conf`'s `builders` syntax.
    #[arg(long)]
    builders: Option<String>,
    /// Set a Nix configuration setting for these calls, e.g. `--option
    /// keep-going true` — the same two-value form `nix` itself uses, not
    /// `name=value`. May be repeated.
    #[arg(long = "option", num_args = 2, value_names = ["name", "value"])]
    options: Vec<String>,
    /// Allow access to mutable paths and repositories during evaluation.
    #[arg(long)]
    impure: bool,
    /// Disable substituters and consider all previously downloaded files up-to-date.
    #[arg(long)]
    offline: bool,
    /// Consider all previously downloaded files out-of-date.
    #[arg(long)]
    refresh: bool,
    /// Keep going as far as possible after a build fails.
    #[arg(short = 'k', long)]
    keep_going: bool,
    /// Fall back to building from source if a substitution fails.
    #[arg(long)]
    fallback: bool,
    /// Print a stack trace when evaluation fails.
    #[arg(long)]
    show_trace: bool,
}

impl NixPassthroughArgs {
    fn to_args(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.print_build_logs {
            out.push("--print-build-logs".to_string());
        }
        if let Some(j) = &self.max_jobs {
            out.push("--max-jobs".to_string());
            out.push(j.clone());
        }
        if let Some(c) = &self.cores {
            out.push("--cores".to_string());
            out.push(c.clone());
        }
        if let Some(b) = &self.builders {
            out.push("--builders".to_string());
            out.push(b.clone());
        }
        for pair in self.options.chunks(2) {
            out.push("--option".to_string());
            out.extend(pair.iter().cloned());
        }
        if self.impure {
            out.push("--impure".to_string());
        }
        if self.offline {
            out.push("--offline".to_string());
        }
        if self.refresh {
            out.push("--refresh".to_string());
        }
        if self.keep_going {
            out.push("--keep-going".to_string());
        }
        if self.fallback {
            out.push("--fallback".to_string());
        }
        if self.show_trace {
            out.push("--show-trace".to_string());
        }
        out
    }
}

/// Strategy knobs shared by `replace` and every `edit_*` subcommand,
/// controlling propagation through the closure above whatever's replaced/edited.
#[derive(Args)]
struct StrategyArgs {
    /// Report what would be grafted/rebuilt without touching the store.
    #[arg(long)]
    dry_run: bool,
    /// Rebuild affected paths (real dependency substitution + sandboxed
    /// rebuild) instead of the default blind NAR byte-substitution graft.
    #[arg(long)]
    rebuild: bool,
    /// Never touch this path, no matter what changed beneath it —
    /// propagation stops there. May be repeated. Use this for anything a
    /// blind path-substitution could corrupt, e.g. a NixOS closure's
    /// embedded store database (it records NAR hashes as separate text a
    /// graft never updates, so grafting it would desync those hashes).
    #[arg(long = "cutoff")]
    cutoffs: Vec<PathBuf>,
    /// If this path changes, always rebuild it regardless of `--rebuild`.
    /// May be repeated.
    #[arg(long = "force-rebuild")]
    force_rebuild_paths: Vec<PathBuf>,
    /// The mirror of `--force-rebuild`: always graft this path even if
    /// `--rebuild` is set globally. May be repeated.
    #[arg(long = "force-graft")]
    force_graft_paths: Vec<PathBuf>,
    /// Open `$EDITOR` on a git-rebase `-i`-style list of every path that
    /// would be affected, one per line as `<strategy> <path>` (graft,
    /// rebuild, or cutoff) — edit the strategy words and save to apply.
    #[arg(short = 'i', long)]
    interactive: bool,
    /// Create a GC-root symlink at this path pointing at the result, like
    /// `nix build -o`. Every build here otherwise passes `--no-link`, so
    /// without this the result isn't protected from a concurrent GC.
    #[arg(short = 'o', long = "out-link")]
    out_link: Option<PathBuf>,
    /// Write an HTML report (dependency graph + details table of
    /// everything grafted/rebuilt/explicitly replaced) to `<dir>/index.html`.
    /// Works under `--dry-run` too.
    #[arg(long)]
    report: Option<PathBuf>,
    /// Embed nix-diff (derivation diff, where one applies) and diffoscope
    /// (built-artifact diff) output per node in the report. Only
    /// meaningful alongside `--report`; real per-node subprocess cost, so
    /// it's a separate flag rather than implied by `--report` alone.
    #[arg(long)]
    report_diff: bool,
    #[command(flatten)]
    nix_common: NixPassthroughArgs,
}

impl StrategyArgs {
    /// `nix_common`'s flags, rendered to CLI tokens, ahead of whatever
    /// trailing `-- <nix args>` the caller also supplied — the only two
    /// sources `nix_args` ever has, merged once here.
    fn merged_nix_args(&self, trailing: Vec<String>) -> Vec<String> {
        let mut args = self.nix_common.to_args();
        args.extend(trailing);
        args
    }

    fn opts<'a>(&'a self, nix_args: &'a [String]) -> ReplaceOptions<'a> {
        ReplaceOptions {
            dry_run: self.dry_run,
            nix_args,
            full_rebuild: self.rebuild,
            cutoffs: &self.cutoffs,
            force_rebuild: &self.force_rebuild_paths,
            force_graft: &self.force_graft_paths,
            interactive: self.interactive,
            report: self.report.as_deref(),
            report_diff: self.report_diff,
        }
    }
}

/// Both flags below produce one or more `(old, new)` pairs fed into the
/// exact same closure walk — they differ only in *how* the pair is
/// produced, so they combine freely in a single invocation and graft
/// together in one pass. Shared by `Replace` and `NixosSystem` since both
/// end up calling [`collect_pairs`] with these same two fields.
#[derive(Args)]
struct TransformArgs {
    /// Override `old` with `new` throughout the closure — the same name
    /// `nix`'s own `--override-input` uses for "swap this specific thing".
    /// `old`/`new` are installables, same forms as `closure-root`. May be
    /// repeated; may be combined with --edit.
    #[arg(long = "override", num_args = 2, value_names = ["old", "new"])]
    overrides: Vec<String>,
    /// Like --override, but `name` is a bare package name (or
    /// `name-version`) instead of an exact store path — resolved by
    /// searching the closure, the same matching `graft find` uses. Refuses
    /// (printing every candidate) if more than one distinct path matches;
    /// pass a `name-version` query or fall back to an exact --override to
    /// disambiguate. May be repeated; may be combined with --override/--edit.
    #[arg(long = "override-name", num_args = 2, value_names = ["name", "new"])]
    override_names: Vec<String>,
    /// Edit `path` by hand and graft the result up through the closure —
    /// what kind of edit depends entirely on what `path` turns out to be,
    /// detected automatically: a `.nix` file sitting on disk means editing
    /// that source and rebuilding the attribute it names; a bare `.drv`
    /// means editing its derivation JSON (env/builder/args) and rebuilding
    /// just that node; anything else means editing a file inside its
    /// already-built output. `selector` means whatever's appropriate for
    /// that kind — an attribute, an output name, or a subpath — or pass
    /// `.` when none applies. May be repeated; may be combined with
    /// --override.
    #[arg(long = "edit", num_args = 2, value_names = ["path", "selector"])]
    edit: Vec<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print a shell completion script to standard output, e.g.
    /// `graft completions bash > /etc/bash_completion.d/graft` or `graft
    /// completions zsh > "${fpath[1]}/_graft"`.
    Completions { shell: Shell },
    /// Search the closure of <closure-root> for a path matching <name> —
    /// read-only, same matching `--override-name` uses, so you can see
    /// what it would resolve to (or why it's ambiguous) before committing
    /// to an override.
    Find {
        /// A store path, flake reference, `.drv` path, or `file.nix[#attr]`
        /// installable — built automatically if not already realized.
        closure_root: String,
        /// Package name (or `name-version`) to search for.
        name: String,
    },
    /// Replace, edit, or rebuild one or more things, grafting every result
    /// up through the closure rooted at <closure-root> in a single pass.
    Replace {
        /// A store path, flake reference, `.drv` path, or `file.nix[#attr]`
        /// installable — built automatically if not already realized.
        closure_root: String,
        #[command(flatten)]
        transforms: TransformArgs,
        #[command(flatten)]
        strategy: StrategyArgs,
        /// Extra flags forwarded to the underlying `nix build` calls that
        /// actually realise each graft/rebuild, e.g. `-- --eval-store <url>`.
        #[arg(last = true)]
        nix_args: Vec<String>,
    },
    /// `replace`, defaulted to a NixOS system profile instead of a
    /// closure-root you have to already know the path of.
    NixosSystem {
        /// The profile to graft and (if --switch is given) update — same
        /// profile `nixos-rebuild` itself operates on. Defaults to the live
        /// system; pass a different path to target a specific generation or
        /// a mounted image's system closure.
        #[arg(long, default_value = "/nix/var/nix/profiles/system")]
        profile: String,
        #[command(flatten)]
        transforms: TransformArgs,
        #[command(flatten)]
        strategy: StrategyArgs,
        /// Register the graft's result as a new generation of --profile and
        /// activate it via `switch-to-configuration`, the same action names
        /// `nixos-rebuild` uses. Without this, the new path is only reported
        /// — nothing on the system changes.
        #[arg(long)]
        switch: Option<nixos_system::SwitchAction>,
        #[arg(last = true)]
        nix_args: Vec<String>,
    },
}

/// Resolves every transform flag in `transforms` into `(old, new)` pairs
/// and gathers them into one list — see [`TransformArgs`]'s doc comment
/// for why this is the one place both converge. The closure membership
/// check an `--edit` that turns out to be a file-edit needs is computed at
/// most once, not once per occurrence, since several may be combined in
/// one invocation.
fn collect_pairs(
    closure_root: &Path,
    nix_args: &[String],
    transforms: TransformArgs,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    let TransformArgs {
        overrides,
        override_names,
        edit,
    } = transforms;
    let mut pairs = Vec::new();
    let mut closure_cache: Option<Vec<PathBuf>> = None;
    for pair in overrides.chunks(2) {
        pairs.extend(resolve_override(&pair[0], &pair[1], nix_args)?);
    }
    for pair in override_names.chunks(2) {
        pairs.push(resolve_override_name(
            &pair[0],
            &pair[1],
            closure_root,
            nix_args,
            &mut closure_cache,
        )?);
    }
    for pair in edit.chunks(2) {
        pairs.extend(detect_edit(
            &pair[0],
            &pair[1],
            closure_root,
            nix_args,
            &mut closure_cache,
        )?);
    }
    if pairs.is_empty() {
        bail!("at least one of --override or --edit is required");
    }
    for (old, new) in &pairs {
        derivation::warn_on_soname_mismatch(old, new);
    }
    Ok(pairs)
}

/// Resolves one `--override <old> <new>` occurrence to one or more pairs.
/// Ordinarily exactly one, both sides resolved as installables — but
/// `old`/`new` both ending in `^*` (nix's own "all outputs" selector)
/// pairs up every output the two installables have in common by name
/// instead, so replacing a multi-output package doesn't need one
/// `--override` per output. An output present in `old` but missing from
/// `new` is skipped with a warning, not a hard failure — nothing in the
/// closure may ever reference it anyway.
fn resolve_override(old: &str, new: &str, nix_args: &[String]) -> Result<Vec<(PathBuf, PathBuf)>> {
    let (old_all, new_all) = (old.ends_with("^*"), new.ends_with("^*"));
    if old_all != new_all {
        bail!("--override {old} {new}: either both sides use `^*` (all outputs) or neither does");
    }
    if !old_all {
        return Ok(vec![(
            installable::resolve(old, nix_args)?,
            installable::resolve(new, nix_args)?,
        )]);
    }
    let old_outputs = derivation::resolve_outputs(old, nix_args)?;
    let new_outputs = derivation::resolve_outputs(new, nix_args)?;
    let mut pairs = Vec::new();
    for (name, old_path) in &old_outputs {
        match new_outputs.get(name) {
            Some(new_path) => pairs.push((old_path.clone(), new_path.clone())),
            None => eprintln!("warning: {new} has no output named `{name}` (present in {old}) — skipping that output"),
        }
    }
    Ok(pairs)
}

/// Resolves `--override-name <name> <new>` to one exact `(old, new)` pair by
/// searching the closure for a path matching `name` (see
/// [`store::find_by_name`]) — succeeds only if exactly one distinct path
/// matches; refuses (listing every candidate) otherwise, since silently
/// picking among genuinely different packages/versions would be a much
/// bigger, likely-wrong change than the caller asked for.
fn resolve_override_name(
    name: &str,
    new: &str,
    closure_root: &Path,
    nix_args: &[String],
    closure_cache: &mut Option<Vec<PathBuf>>,
) -> Result<(PathBuf, PathBuf)> {
    let closure = ensure_closure(closure_cache, closure_root)?;
    let matches = store::find_by_name(closure, name)?;
    match matches.as_slice() {
        [] => bail!(
            "--override-name {name} {new}: no path in the closure of {} matches `{name}`",
            closure_root.display()
        ),
        [one] => Ok((one.clone(), installable::resolve(new, nix_args)?)),
        many => {
            let list = many
                .iter()
                .map(|p| format!("  {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "--override-name {name} {new}: ambiguous, {} paths in the closure of {} match `{name}`:\n{list}\n\
                 pick one with an exact --override <old> {new}, or narrow the query with a version \
                 (e.g. `{name}-<version>`)",
                many.len(),
                closure_root.display()
            );
        }
    }
}

fn ensure_closure<'a>(
    cache: &'a mut Option<Vec<PathBuf>>,
    closure_root: &Path,
) -> Result<&'a [PathBuf]> {
    if cache.is_none() {
        *cache = Some(store::closure(closure_root)?);
    }
    Ok(cache.as_ref().unwrap())
}

/// Detects what kind of edit `--edit <path> <selector>` means, mostly from
/// what `path` itself turns out to be (never ambiguously more than one of
/// these, so no guessing or backtracking needed) — with one exception
/// noted below where `selector` breaks a tie:
/// 0. `path` ends in `^*` (nix's own "all outputs" selector) -> drv-edit,
///    once, producing a pair for *every* output instead of just one.
///    `selector` isn't meaningful here (there's no single output or
///    subpath left to name) and must be `.`.
/// 1. `path` is a `.nix` file sitting on disk (not a store path) ->
///    nix-edit, `selector` an attribute (`.` for none).
/// 2. `path` resolves to a bare `.drv` -> drv-edit, `selector` an output
///    name (`.` to infer it, which only works if there's exactly one).
/// 3. `path` resolves to a plain output whose own deriver has an output
///    literally named `selector`, and `selector` *isn't* also an existing
///    subpath inside it -> drv-edit anyway, resolving the deriver
///    automatically (what `--override-drv` used to do for free on a
///    plain output path). Subpath wins when both would apply.
/// 4. Otherwise -> file-edit, `selector` a subpath inside the resolved
///    output (`.` for the whole tree).
fn detect_edit(
    path: &str,
    selector: &str,
    closure_root: &Path,
    nix_args: &[String],
    closure_cache: &mut Option<Vec<PathBuf>>,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    if let Some(base) = path.strip_suffix("^*") {
        if selector != "." {
            bail!("--edit {path} {selector}: `selector` isn't meaningful with `^*` (all outputs); pass `.`");
        }
        let resolved = installable::resolve(base, nix_args)?;
        return edit_drv::produce_pairs_all_outputs(&resolved, nix_args);
    }

    if installable::is_legacy_nix_file(Path::new(path)) {
        let installable = if selector == "." {
            path.to_string()
        } else {
            format!("{path}#{selector}")
        };
        return Ok(vec![edit_nix::produce_pair(&installable, nix_args)?]);
    }

    let resolved = installable::resolve(path, nix_args)?;

    if resolved.extension().and_then(|e| e.to_str()) == Some("drv") {
        let output = if selector == "." {
            None
        } else {
            Some(selector)
        };
        return Ok(vec![edit_drv::produce_pair(&resolved, output, nix_args)?]);
    }

    // A plain output path never needs `selector` to disambiguate an
    // output (it's already unambiguous) — but that's exactly the shape
    // `--override-drv` used to let you edit a derivation via its already-
    // built output, with no need to look up its `.drv` separately. Keeping
    // that reachable from one auto-detecting flag means checking, only
    // when `selector` isn't an existing subpath: does it instead name one
    // of this path's own deriver's outputs? Subpath wins when both apply,
    // since it's the more literal reading of "edit this path".
    if selector != "."
        && !resolved.join(selector).exists()
        && derivation::has_output(&resolved, selector)
    {
        return Ok(vec![edit_drv::produce_pair(
            &resolved,
            Some(selector),
            nix_args,
        )?]);
    }

    let closure = ensure_closure(closure_cache, closure_root)?;
    let subpath = if selector == "." {
        None
    } else {
        Some(Path::new(selector))
    };
    Ok(vec![edit_file::produce_pair(
        closure_root,
        closure,
        &resolved,
        subpath,
        nix_args,
    )?])
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    log::set_verbose(cli.verbose);
    let (result, out_link, nix_args) = match cli.command {
        Cmd::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "graft", &mut std::io::stdout());
            return Ok(());
        }
        Cmd::Find { closure_root, name } => return run_find(&closure_root, &name),
        Cmd::Replace {
            closure_root,
            transforms,
            strategy,
            nix_args,
        } => {
            let nix_args = strategy.merged_nix_args(nix_args);
            let closure_root = installable::resolve(&closure_root, &nix_args)?;
            let pairs = collect_pairs(&closure_root, &nix_args, transforms)?;
            let out_link = out_link_unless_dry_run(&strategy);
            (
                replace::replace(&closure_root, &pairs, &strategy.opts(&nix_args))?,
                out_link,
                nix_args,
            )
        }
        Cmd::NixosSystem {
            profile,
            transforms,
            strategy,
            switch,
            nix_args,
        } => {
            let nix_args = strategy.merged_nix_args(nix_args);
            let closure_root = installable::resolve(&profile, &nix_args).with_context(|| {
                format!(
                    "failed to resolve `{profile}` — is this a NixOS system? Pass --profile <path> to \
                     target a different one (a specific generation, or a mounted image's system closure)"
                )
            })?;
            let pairs = collect_pairs(&closure_root, &nix_args, transforms)?;
            let out_link = out_link_unless_dry_run(&strategy);
            (
                nixos_system::run(
                    &profile,
                    &closure_root,
                    &pairs,
                    &strategy.opts(&nix_args),
                    switch,
                )?,
                out_link,
                nix_args,
            )
        }
    };
    if let Some(link) = out_link {
        derivation::add_out_link(&result.new_root, &link, &nix_args)?;
        provenance::record(&link, &result.new_root)?;
    }
    println!("{}", result.new_root.display());
    Ok(())
}

/// `graft find`: lists every path in the closure matching `name` (same
/// matching `--override-name` uses), each with one direct consumer so
/// "which occurrence is this" has an answer — purely read-only.
fn run_find(closure_root: &str, name: &str) -> Result<()> {
    let closure_root = installable::resolve(closure_root, &[])?;
    let closure = store::closure(&closure_root)?;
    let matches = store::find_by_name(&closure, name)?;
    if matches.is_empty() {
        println!(
            "no matches for `{name}` in the closure of {}",
            closure_root.display()
        );
        return Ok(());
    }
    let refs = store::references_many(&closure)?;
    let mut consumer_of: HashMap<&PathBuf, &PathBuf> = HashMap::new();
    for (p, rs) in &refs {
        for r in rs {
            consumer_of.entry(r).or_insert(p);
        }
    }
    for m in &matches {
        match consumer_of.get(m) {
            Some(consumer) => println!("{}  (consumed by {})", m.display(), consumer.display()),
            None => println!(
                "{}  (not directly referenced by anything else in this closure)",
                m.display()
            ),
        }
    }
    Ok(())
}

/// `--out-link` only makes sense against something actually built — under
/// `--dry-run` nothing changed, so there's nothing new to root.
fn out_link_unless_dry_run(strategy: &StrategyArgs) -> Option<PathBuf> {
    if strategy.dry_run {
        None
    } else {
        strategy.out_link.clone()
    }
}
