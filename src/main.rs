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
use clap::{Args, Parser, Subcommand};
use replace::ReplaceOptions;
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

/// Every transform flag below produces one or more `(old, new)` pairs fed
/// into the exact same closure walk — they differ only in *how* the pair
/// is produced, so all four combine freely in a single invocation and
/// graft together in one pass. Shared by `Replace` and `NixosSystem` since
/// both end up calling [`collect_pairs`] with these same four fields.
#[derive(Args)]
struct TransformArgs {
    /// Override `old` with `new` throughout the closure — the same name
    /// `nix`'s own `--override-input` uses for "swap this specific thing".
    /// `old`/`new` are installables, same forms as `closure-root`. May be
    /// repeated; may be combined with --edit/--edit-drv/--edit-nix.
    #[arg(long = "override", num_args = 2, value_names = ["old", "new"])]
    overrides: Vec<String>,
    /// Edit a file inside an already-built store path and graft the
    /// result up through the closure. `subpath` is relative to `path`'s
    /// own root; pass `.` for the whole tree. May be repeated; may be
    /// combined with --override/--edit-drv/--edit-nix.
    #[arg(long = "edit", num_args = 2, value_names = ["path", "subpath"])]
    edit: Vec<String>,
    /// Edit a derivation's JSON (env/builder/args) and rebuild just that
    /// node. `output` disambiguates which output to edit when `path` is a
    /// bare `.drv` with more than one (ignored otherwise, since a plain
    /// output path is already unambiguous) — pass `.` to infer it, which
    /// only works if there's exactly one. May be repeated; may be combined
    /// with --override/--edit/--edit-nix.
    #[arg(long = "edit-drv", num_args = 2, value_names = ["path", "output"])]
    edit_drv: Vec<String>,
    /// Edit the .nix file backing a file-based installable
    /// (`path/to/file.nix[#attr]`) and rebuild just that attribute. May be
    /// repeated; may be combined with --override/--edit/--edit-drv.
    #[arg(long = "edit-nix", value_name = "file.nix[#attr]")]
    edit_nix: Vec<String>,
}

#[derive(Subcommand)]
enum Cmd {
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
/// for why this is the one place all four converge. `--edit`'s closure
/// membership check is computed at most once, not once per `--edit`, since
/// several may be combined in one invocation.
fn collect_pairs(closure_root: &Path, nix_args: &[String], transforms: TransformArgs) -> Result<Vec<(PathBuf, PathBuf)>> {
    let TransformArgs { overrides, edit, edit_drv, edit_nix } = transforms;
    let mut pairs = Vec::new();
    for pair in overrides.chunks(2) {
        pairs.push((installable::resolve(&pair[0], nix_args)?, installable::resolve(&pair[1], nix_args)?));
    }
    if !edit.is_empty() {
        let closure = store::closure(closure_root)?;
        for pair in edit.chunks(2) {
            let path = installable::resolve(&pair[0], nix_args)?;
            let subpath = if pair[1] == "." { None } else { Some(PathBuf::from(&pair[1])) };
            pairs.push(edit_file::produce_pair(closure_root, &closure, &path, subpath.as_deref())?);
        }
    }
    for pair in edit_drv.chunks(2) {
        let path = installable::resolve(&pair[0], nix_args)?;
        let output = if pair[1] == "." { None } else { Some(pair[1].as_str()) };
        pairs.push(edit_drv::produce_pair(&path, output, nix_args)?);
    }
    for installable_str in &edit_nix {
        pairs.push(edit_nix::produce_pair(installable_str, nix_args)?);
    }
    if pairs.is_empty() {
        bail!("at least one of --override, --edit, --edit-drv, or --edit-nix is required");
    }
    Ok(pairs)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    log::set_verbose(cli.verbose);
    let (result, out_link, nix_args) = match cli.command {
        Cmd::Replace { closure_root, transforms, strategy, nix_args } => {
            let nix_args = strategy.merged_nix_args(nix_args);
            let closure_root = installable::resolve(&closure_root, &nix_args)?;
            let pairs = collect_pairs(&closure_root, &nix_args, transforms)?;
            let out_link = out_link_unless_dry_run(&strategy);
            (replace::replace(&closure_root, &pairs, &strategy.opts(&nix_args))?, out_link, nix_args)
        }
        Cmd::NixosSystem { profile, transforms, strategy, switch, nix_args } => {
            let nix_args = strategy.merged_nix_args(nix_args);
            let closure_root = installable::resolve(&profile, &nix_args).with_context(|| {
                format!(
                    "failed to resolve `{profile}` — is this a NixOS system? Pass --profile <path> to \
                     target a different one (a specific generation, or a mounted image's system closure)"
                )
            })?;
            let pairs = collect_pairs(&closure_root, &nix_args, transforms)?;
            let out_link = out_link_unless_dry_run(&strategy);
            (nixos_system::run(&profile, &closure_root, &pairs, &strategy.opts(&nix_args), switch)?, out_link, nix_args)
        }
    };
    if let Some(link) = out_link {
        derivation::add_out_link(&result.new_root, &link, &nix_args)?;
        provenance::record(&link, &result.new_root)?;
    }
    println!("{}", result.new_root.display());
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
