use crate::replace::{ReplaceOptions, ReplaceResult};
use crate::{installable, log, replace};
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

/// `graft replace`, defaulted to a NixOS system profile instead of requiring
/// the caller to already know its store path. `profile` doubles as both the
/// closure to read (resolved the same way `/run/current-system`-style
/// symlinks always have been) and, if `switch` is given, the profile
/// `nix-env --set` registers the graft's result into — the same profile
/// `nixos-rebuild` itself operates on.
pub fn run(
    profile: &str,
    replacements: &[(String, String)],
    opts: &ReplaceOptions,
    switch: Option<SwitchAction>,
) -> Result<ReplaceResult> {
    let closure_root = installable::resolve(profile, opts.nix_args).with_context(|| {
        format!(
            "failed to resolve `{profile}` — is this a NixOS system? Pass --profile <path> to \
             target a different one (a specific generation, or a mounted image's system closure)"
        )
    })?;
    let replacements: Vec<(PathBuf, PathBuf)> = replacements
        .iter()
        .map(|(old, new)| Ok((installable::resolve(old, opts.nix_args)?, installable::resolve(new, opts.nix_args)?)))
        .collect::<Result<_>>()?;

    let result = replace::replace(&closure_root, &replacements, opts)?;
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
        bail!("nix-env --profile {profile} --set {} failed", new_root.display());
    }

    let switch_bin = new_root.join("bin/switch-to-configuration");
    log::v(format!("running {} {}", switch_bin.display(), action.as_str()));
    let status = Command::new(&switch_bin)
        .arg(action.as_str())
        .status()
        .with_context(|| format!("failed to spawn {}", switch_bin.display()))?;
    if !status.success() {
        bail!("{} {} failed", switch_bin.display(), action.as_str());
    }
    Ok(())
}
