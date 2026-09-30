mod derivation;
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

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use replace::ReplaceOptions;
use std::path::PathBuf;

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
            report: self.report.as_deref(),
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Replace all of `old` with `new`, in the context of the closure rooted at <closure-root>.
    Replace {
        /// A store path, flake reference, `.drv` path, or `file.nix[#attr]`
        /// installable — built automatically if not already realized.
        closure_root: String,
        /// old=new installable pair; may be repeated. Same installable
        /// forms as `closure-root`.
        #[arg(long = "replace", value_parser = parse_pair, required = true)]
        replacements: Vec<(String, String)>,
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
    /// `replace`, defaulted to a NixOS system profile instead of a
    /// closure-root you have to already know the path of.
    NixosSystem {
        /// The profile to graft and (if --switch is given) update — same
        /// profile `nixos-rebuild` itself operates on. Defaults to the live
        /// system; pass a different path to target a specific generation or
        /// a mounted image's system closure.
        #[arg(long, default_value = "/nix/var/nix/profiles/system")]
        profile: String,
        /// old=new installable pair; may be repeated. Same installable
        /// forms as `replace`'s.
        #[arg(long = "replace", value_parser = parse_pair, required = true)]
        replacements: Vec<(String, String)>,
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

#[derive(Subcommand)]
enum EditCmd {
    /// Edit a file inside an already-built store path.
    File {
        closure_root: String,
        path: String,
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
        closure_root: String,
        /// A `.drv` path, a plain store output path (its deriver is looked
        /// up automatically), or any other installable.
        path: String,
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
        closure_root: String,
        installable: String,
        #[command(flatten)]
        strategy: StrategyArgs,
        /// Extra flags forwarded to the `nix build` call for the edited
        /// attribute and every graft built on top of it.
        #[arg(last = true)]
        nix_args: Vec<String>,
    },
}

fn parse_pair(s: &str) -> Result<(String, String), String> {
    let (old, new) = s
        .split_once('=')
        .ok_or_else(|| format!("expected old=new, got `{s}`"))?;
    Ok((old.to_string(), new.to_string()))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    log::set_verbose(cli.verbose);
    let (result, out_link) = match cli.command {
        Cmd::Replace { closure_root, replacements, strategy, nix_args } => {
            let closure_root = installable::resolve(&closure_root, &nix_args)?;
            let replacements: Vec<(PathBuf, PathBuf)> = replacements
                .iter()
                .map(|(old, new)| Ok((installable::resolve(old, &nix_args)?, installable::resolve(new, &nix_args)?)))
                .collect::<Result<_>>()?;
            let out_link = out_link_unless_dry_run(&strategy);
            (replace::replace(&closure_root, &replacements, &strategy.opts(&nix_args))?, out_link)
        }
        Cmd::Edit { target } => match target {
            EditCmd::File { closure_root, path, subpath, strategy, nix_args } => {
                let closure_root = installable::resolve(&closure_root, &nix_args)?;
                let path = installable::resolve(&path, &nix_args)?;
                let out_link = out_link_unless_dry_run(&strategy);
                (edit_file::run(&closure_root, &path, subpath.as_deref(), &strategy.opts(&nix_args))?, out_link)
            }
            EditCmd::Drv { closure_root, path, output, strategy, nix_args } => {
                let closure_root = installable::resolve(&closure_root, &nix_args)?;
                let path = installable::resolve(&path, &nix_args)?;
                let out_link = out_link_unless_dry_run(&strategy);
                (edit_drv::run(&closure_root, &path, output.as_deref(), &strategy.opts(&nix_args))?, out_link)
            }
            EditCmd::Nix { closure_root, installable, strategy, nix_args } => {
                let closure_root = crate::installable::resolve(&closure_root, &nix_args)?;
                let out_link = out_link_unless_dry_run(&strategy);
                (edit_nix::run(&closure_root, &installable, &strategy.opts(&nix_args))?, out_link)
            }
        },
        Cmd::NixosSystem { profile, replacements, strategy, switch, nix_args } => {
            let out_link = out_link_unless_dry_run(&strategy);
            (nixos_system::run(&profile, &replacements, &strategy.opts(&nix_args), switch)?, out_link)
        }
    };
    if let Some(link) = out_link {
        derivation::add_out_link(&result.new_root, &link)?;
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
