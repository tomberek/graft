use crate::replace::{ReplaceOptions, ReplaceResult};
use crate::{log, replace};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// `switch-to-configuration`'s own action names, reused here so `--switch`
/// matches what `nixos-rebuild` users already expect.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum SwitchAction {
    Test,
    Switch,
    Boot,
}

impl SwitchAction {
    fn as_str(self) -> &'static str {
        match self {
            SwitchAction::Test => "test",
            SwitchAction::Switch => "switch",
            SwitchAction::Boot => "boot",
        }
    }
}

/// `graft replace`, with `closure_root` already resolved from `profile` and
/// `replacements` already resolved from whatever mix of
/// `--override`/`--edit` was given — see
/// `main.rs`'s `collect_pairs`. `profile` itself is still needed here,
/// for `activate()`'s `nix-env --set`, which is the other half of what
/// defaulting to a NixOS system profile means: not just reading its
/// closure, but (if `switch` is given) registering the result back into
/// that same profile, the same profile `nixos-rebuild` itself operates on.
pub fn run(
    profile: &str,
    closure_root: &Path,
    replacements: &[(PathBuf, PathBuf)],
    opts: &ReplaceOptions,
    switch: Option<SwitchAction>,
) -> Result<ReplaceResult> {
    let result = replace::replace(closure_root, replacements, opts)?;
    if opts.dry_run {
        return Ok(result);
    }

    match switch {
        Some(action) => activate(profile, &result.new_root, action)?,
        None => eprintln!(
            "not switching (pass --switch test|switch|boot to activate). To apply manually:\n  \
             nix-env --profile {profile} --set {}\n  {}/bin/switch-to-configuration switch",
            result.new_root.display(),
            result.new_root.display()
        ),
    }
    Ok(result)
}

fn activate(profile: &str, new_root: &Path, action: SwitchAction) -> Result<()> {
    log::v(format!("registering {} as {profile}", new_root.display()));
    let status = Command::new("nix-env")
        .args(["--profile", profile, "--set"])
        .arg(new_root)
        .status()
        .context("failed to spawn nix-env --set")?;
    if !status.success() {
        bail!(
            "nix-env --profile {profile} --set {} failed",
            new_root.display()
        );
    }

    let switch_bin = new_root.join("bin/switch-to-configuration");
    log::v(format!(
        "running {} {}",
        switch_bin.display(),
        action.as_str()
    ));
    let status = Command::new(&switch_bin)
        .arg(action.as_str())
        .status()
        .with_context(|| format!("failed to spawn {}", switch_bin.display()))?;
    if !status.success() {
        bail!("{} {} failed", switch_bin.display(), action.as_str());
    }
    Ok(())
}
