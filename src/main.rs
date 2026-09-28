mod derivation;
mod edit_drv;
mod edit_file;
mod edit_nix;
mod editor;
mod log;
mod rebuild;
mod replace;
mod store;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use replace::ReplaceOptions;
use std::path::PathBuf;

/// Guix-style graft/rewrite prototype for the Nix store.
#[derive(Parser)]
#[command(name = "graft")]
struct Cli {
    /// Narrate what graft itself is doing: paths examined, commands run,
    /// decisions made. Separate from `-- <nix args>`, which controls the
    /// verbosity of the underlying `nix build` calls, not graft's own.
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Cmd,
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
}

impl StrategyArgs {
    fn opts<'a>(&'a self, nix_args: &'a [String]) -> ReplaceOptions<'a> {
        ReplaceOptions {
            dry_run: self.dry_run,
            nix_args,
            full_rebuild: self.rebuild,
            cutoffs: &self.cutoffs,
            force_rebuild: &self.force_rebuild_paths,
            force_graft: &self.force_graft_paths,
            interactive: self.interactive,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Replace all of `old` with `new`, in the context of the closure rooted at <closure-root>.
    Replace {
        closure_root: PathBuf,
        /// old=new store path pair; may be repeated.
        #[arg(long = "replace", value_parser = parse_pair, required = true)]
        replacements: Vec<(PathBuf, PathBuf)>,
        #[command(flatten)]
        strategy: StrategyArgs,
        /// Extra flags forwarded to the underlying `nix build` calls that
        /// actually realise each graft/rebuild, e.g. `-- -Lv --builders ssh://...`.
        #[arg(last = true)]
        nix_args: Vec<String>,
    },
    /// Edit a value inside the closure and graft the result up through it.
    Edit {
        #[command(subcommand)]
        target: EditCmd,
    },
}

#[derive(Subcommand)]
enum EditCmd {
    /// Edit a file inside an already-built store path.
    File {
        closure_root: PathBuf,
        path: PathBuf,
        /// Path within the store item to open; defaults to the whole tree.
        subpath: Option<PathBuf>,
        #[command(flatten)]
        strategy: StrategyArgs,
        /// Extra flags forwarded to the `nix build` calls made while
        /// grafting the edit up through the closure.
        #[arg(last = true)]
        nix_args: Vec<String>,
    },
    /// Edit a derivation's JSON (env/builder/args) and rebuild just that node.
    Drv {
        closure_root: PathBuf,
        /// A `.drv` path, or a plain store output path (its deriver is
        /// looked up automatically).
        path: PathBuf,
        /// Which output to edit, by name (e.g. `dev`, `man`) — only needed
        /// when `path` is a bare `.drv` with more than one output; if `path`
        /// is already a specific output path, the output is unambiguous.
        #[arg(long)]
        output: Option<String>,
        #[command(flatten)]
        strategy: StrategyArgs,
        /// Extra flags forwarded to the `nix build` calls for the edited
        /// node itself and every graft built on top of it.
        #[arg(last = true)]
        nix_args: Vec<String>,
    },
    /// Edit the .nix file backing an installable and rebuild just that attribute.
    Nix {
        closure_root: PathBuf,
        installable: String,
        #[command(flatten)]
        strategy: StrategyArgs,
        /// Extra flags forwarded to the `nix build` call for the edited
        /// attribute and every graft built on top of it.
        #[arg(last = true)]
        nix_args: Vec<String>,
    },
}

fn parse_pair(s: &str) -> Result<(PathBuf, PathBuf), String> {
    let (old, new) = s
        .split_once('=')
        .ok_or_else(|| format!("expected old=new, got `{s}`"))?;
    Ok((PathBuf::from(old), PathBuf::from(new)))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    log::set_verbose(cli.verbose);
    let result = match cli.command {
        Cmd::Replace { closure_root, replacements, strategy, nix_args } => {
            replace::replace(&closure_root, &replacements, &strategy.opts(&nix_args))?
        }
        Cmd::Edit { target } => match target {
            EditCmd::File { closure_root, path, subpath, strategy, nix_args } => {
                edit_file::run(&closure_root, &path, subpath.as_deref(), &strategy.opts(&nix_args))?
            }
            EditCmd::Drv { closure_root, path, output, strategy, nix_args } => {
                edit_drv::run(&closure_root, &path, output.as_deref(), &strategy.opts(&nix_args))?
            }
            EditCmd::Nix { closure_root, installable, strategy, nix_args } => {
                edit_nix::run(&closure_root, &installable, &strategy.opts(&nix_args))?
            }
        },
    };
    for (old, new) in &result.grafted {
        eprintln!("grafted {} -> {}", old.display(), new.display());
    }
    println!("{}", result.new_root.display());
    Ok(())
}
