use crate::replace::{ReplaceOptions, ReplaceResult};
use crate::{editor, log, replace, store};
use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Best-effort: record `installable`'s current output, open `$EDITOR` on the
/// `.nix` file backing it, rebuild just that attribute, and graft the result
/// up through `closure_root`. Only supports file-based installables
/// (`path/to/file.nix` or `path/to/file.nix#attr`, in the `-f`/`nix-build`
/// sense) and does not attempt to seek the editor to `attr`'s exact location.
pub fn run(closure_root: &Path, installable: &str, opts: &ReplaceOptions) -> Result<ReplaceResult> {
    store::require_output_path(closure_root)?;
    store::canonicalize(closure_root)?;
    let (file, attr) = parse_installable(installable);
    if !file.exists() {
        bail!("`graft edit nix` only supports file-based installables; could not find `{}` on disk", file.display());
    }
    let old = current_output(&file, attr.as_deref())?;
    log::v(format!("current output of {}: {}", installable, old.display()));
    log::v(format!("opening $EDITOR on {}", file.display()));
    editor::edit(&file)?;
    let new = build(&file, attr.as_deref(), opts.nix_args)?;
    log::v(format!("rebuilt attribute: {} -> {}", old.display(), new.display()));
    replace::replace(closure_root, &[(old, new)], opts)
}

/// `file.nix#attr` -> (`file.nix`, Some(`attr`)); `file.nix` -> (`file.nix`, None).
fn parse_installable(installable: &str) -> (PathBuf, Option<String>) {
    match installable.split_once('#') {
        Some((f, a)) => (PathBuf::from(f), Some(a.to_string())),
        None => (PathBuf::from(installable), None),
    }
}

fn current_output(file: &Path, attr: Option<&str>) -> Result<PathBuf> {
    let mut cmd = Command::new("nix");
    cmd.args(["path-info", "-f"]).arg(file);
    if let Some(a) = attr {
        cmd.arg(a);
    }
    log::v(format!(
        "running: nix path-info -f {}{}",
        file.display(),
        attr.map(|a| format!(" {a}")).unwrap_or_default()
    ));
    let output = cmd.output().context("failed to run `nix path-info -f`")?;
    if !output.status.success() {
        bail!(
            "`nix path-info -f {}{}` failed (has it been built yet? `edit nix` only works on \
             installables that already have a realized output): {}",
            file.display(),
            attr.map(|a| format!(" {a}")).unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(|l| PathBuf::from(l.trim()))
        .with_context(|| format!("`nix path-info -f {}` produced no output", file.display()))
}

fn build(file: &Path, attr: Option<&str>, nix_args: &[String]) -> Result<PathBuf> {
    let mut cmd = Command::new("nix");
    cmd.args(["build", "-f"]).arg(file);
    if let Some(a) = attr {
        cmd.arg(a);
    }
    cmd.args(["--no-link", "--print-out-paths"]).args(nix_args);
    log::v(format!(
        "running: nix build -f {}{} --no-link --print-out-paths{}",
        file.display(),
        attr.map(|a| format!(" {a}")).unwrap_or_default(),
        if nix_args.is_empty() { String::new() } else { format!(" {}", nix_args.join(" ")) }
    ));
    // Inherit stderr so build logs stream live; only stdout needs capturing.
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to spawn `nix build -f`")?;
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("piped stdout")
        .read_to_string(&mut stdout)
        .context("failed to read output of `nix build -f`")?;
    let status = child.wait().context("nix build -f did not exit")?;
    if !status.success() {
        bail!("nix build -f {} failed (see build output above)", file.display());
    }
    stdout
        .lines()
        .next()
        .map(|l| PathBuf::from(l.trim()))
        .with_context(|| format!("nix build -f {} produced no output path", file.display()))
}
