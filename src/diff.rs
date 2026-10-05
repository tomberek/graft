use crate::log;
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

/// Runs `nix-diff <old> <new> --color never`, returning its plain-text
/// output. A nonzero exit is a genuine error here — confirmed empirically,
/// `nix-diff` always exits 0 regardless of whether the two derivations
/// differ, unlike `diffoscope`'s exit-code convention below.
pub fn nix_diff(old_deriver: &Path, new_deriver: &Path) -> Result<String> {
    log::v(format!(
        "running: nix-diff {} {} --color never",
        old_deriver.display(),
        new_deriver.display()
    ));
    let output = Command::new("nix-diff")
        .args(["--color", "never"])
        .arg(old_deriver)
        .arg(new_deriver)
        .output()
        .context("failed to spawn nix-diff (is it on PATH? see flake.nix's devShell)")?;
    if !output.status.success() {
        bail!(
            "nix-diff {} {} failed: {}",
            old_deriver.display(),
            new_deriver.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs `diffoscope --html <out_html> <old> <new>`. `out_html`'s parent
/// directory must already exist — diffoscope itself does not create it,
/// and fails with a raw Python traceback (not a clean error) if it doesn't.
///
/// Unlike `nix_diff`, diffoscope's exit code signals outcome — confirmed
/// empirically: `0` means no differences found, `1` means differences were
/// found, both intended as success. But exit code alone isn't sufficient to
/// tell that apart from a crash: an unhandled Python exception (confirmed
/// empirically, e.g. from the missing-directory case above) *also* exits
/// `1`. So `Some(1)` only counts as success if `out_html` was actually
/// written; a crash before it could do that still surfaces as an error.
pub fn diffoscope_html(old: &Path, new: &Path, out_html: &Path) -> Result<()> {
    log::v(format!(
        "running: diffoscope --html {} {} {}",
        out_html.display(),
        old.display(),
        new.display()
    ));
    let output = Command::new("diffoscope")
        .arg("--html")
        .arg(out_html)
        .arg(old)
        .arg(new)
        .output()
        .context("failed to spawn diffoscope (is it on PATH? see flake.nix's devShell)")?;
    let ok = matches!(output.status.code(), Some(0) | Some(1)) && out_html.exists();
    if !ok {
        bail!(
            "diffoscope --html {} {} {} failed: {}",
            out_html.display(),
            old.display(),
            new.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
