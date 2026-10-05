use crate::{derivation, log, store};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Resolve `s` to a concrete store path — accepting anything `nix build`
/// itself accepts (a flake reference, a `.drv` path) or a legacy
/// expression-file installable (`file.nix` or `file.nix#attr`, in the
/// `-f`/`nix-build` sense), building it if it isn't already. Lets every
/// path-taking argument on the CLI (`closure-root`, `--override old=new`,
/// `edit drv`'s/`edit file`'s `path`) skip the "pre-build it yourself and
/// paste the hash" step.
pub fn resolve(s: &str, nix_args: &[String]) -> Result<PathBuf> {
    let direct = PathBuf::from(s);
    if direct.starts_with(store::STORE_DIR) {
        // A literal store path (an output or a `.drv`, real or not) —
        // return as-is, deliberately unvalidated: callers like `replace`
        // check syntax (e.g. basename length) before requiring existence,
        // and running this through `nix build`/canonicalize here would
        // both short-circuit that ordering and lose `edit drv`'s ability
        // to take a bare `.drv` and disambiguate its output via `--output`.
        return Ok(direct);
    }
    let (file, attr) = match s.split_once('#') {
        Some((f, a)) => (PathBuf::from(f), Some(a.to_string())),
        None => (direct, None),
    };
    if is_legacy_nix_file(&file) {
        log::v(format!("resolving `{s}` as a legacy expression-file installable"));
        let mut args = vec!["-f".to_string(), file.display().to_string()];
        if let Some(a) = attr {
            args.push(a);
        }
        return derivation::nix_build(&args, nix_args);
    }
    log::v(format!("resolving `{s}` as a flake reference / store path / .drv path"));
    derivation::nix_build(&[s.to_string()], nix_args)
}

/// Whether `path` is a `.nix` file sitting on disk — the signal both
/// `resolve` (legacy `-f file.nix [attr]` installables) and bare
/// `--override`'s auto-detection (nix-edit) key off of.
pub fn is_legacy_nix_file(path: &Path) -> bool {
    path.exists() && path.extension().and_then(|e| e.to_str()) == Some("nix")
}
