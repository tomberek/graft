use crate::{derivation, editor, log};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Best-effort: record `installable`'s current output, open `$EDITOR` on
/// the `.nix` file backing it, rebuild just that attribute, and return the
/// `(old, new)` pair — grafting it up through the closure is the caller's
/// job (see `edit_file::produce_pair`'s doc comment). Only supports
/// file-based installables (`path/to/file.nix` or `path/to/file.nix#attr`,
/// in the `-f`/`nix-build` sense) and does not attempt to seek the editor
/// to `attr`'s exact location.
pub fn produce_pair(installable: &str, nix_args: &[String]) -> Result<(PathBuf, PathBuf)> {
    let (file, attr) = parse_installable(installable);
    if !file.exists() {
        bail!("`--edit-nix` only supports file-based installables; could not find `{}` on disk", file.display());
    }
    let old = current_output(&file, attr.as_deref())?;
    log::v(format!("current output of {}: {}", installable, old.display()));
    log::v(format!("opening $EDITOR on {}", file.display()));
    editor::edit(&file)?;
    let new = build(&file, attr.as_deref(), nix_args)?;
    log::v(format!("rebuilt attribute: {} -> {}", old.display(), new.display()));
    Ok((old, new))
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
    let mut args = vec!["-f".to_string(), file.display().to_string()];
    if let Some(a) = attr {
        args.push(a.to_string());
    }
    derivation::nix_build(&args, nix_args)
}
