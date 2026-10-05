use crate::log;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const STORE_DIR: &str = "/nix/store";

/// The 32-character base32 hash prefix length in a Nix store path basename,
/// before the `-name-version` suffix.
const HASH_LEN: usize = 32;

/// Reject `.drv` paths where a build *output* is required: a `.drv`'s own
/// references are a different graph than the output-path closures this
/// tool walks, so accepting one here would silently return meaningless results.
pub fn require_output_path(path: &Path) -> Result<()> {
    if path.extension().and_then(|e| e.to_str()) == Some("drv") {
        bail!(
            "{} is a derivation (.drv) file, not a build output — graft walks realized \
             output closures, not derivation-file closures. Pass the actual store output path \
             instead (e.g. `nix build <installable> --no-link --print-out-paths`).",
            path.display()
        );
    }
    Ok(())
}

/// Resolve `path` to its canonical absolute form: `nix path-info` always
/// returns canonical paths, so a relative path or result symlink (e.g.
/// `./abc`) would otherwise never match. Also doubles as an existence check.
pub fn canonicalize(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).with_context(|| {
        let cwd = std::env::current_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|_| "?".into());
        format!(
            "failed to resolve {} (relative to current directory {cwd}) — check that it exists, \
             that you're in the right directory if it's a relative path, and that it isn't a \
             symlink (e.g. a `nix-build -o` result link) whose target has since been \
             garbage-collected or deleted",
            path.display()
        )
    })
}

pub fn basename(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .with_context(|| format!("path has no basename: {}", path.display()))
}

/// The top-level store item containing `path`, e.g.
/// `/nix/store/HASH-name/bin/foo` -> `/nix/store/HASH-name` — derivation
/// `inputSrcs` must name the store item itself, not a file nested inside it.
pub fn store_root(path: &Path) -> Result<PathBuf> {
    let s = path
        .to_str()
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))?;
    let prefix = format!("{STORE_DIR}/");
    let rest = s
        .strip_prefix(&prefix)
        .with_context(|| format!("{} is not inside {STORE_DIR}", path.display()))?;
    let top = rest
        .split('/')
        .next()
        .with_context(|| format!("could not find a store item in {}", path.display()))?;
    Ok(PathBuf::from(format!("{STORE_DIR}/{top}")))
}

/// The `name-version` suffix of a store path basename, i.e. everything after
/// the fixed-length hash and its separating hyphen.
pub fn store_name(path: &Path) -> Result<String> {
    let base = basename(path)?;
    if base.len() <= HASH_LEN + 1 {
        bail!("store path basename too short to contain a name: {}", base);
    }
    Ok(base[HASH_LEN + 1..].to_string())
}

fn run(cmd: &str, args: &[&str]) -> Result<String> {
    log::v(format!("running: {cmd} {}", args.join(" ")));
    let output = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn `{cmd} {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`{cmd} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Full transitive closure (requisites) of `path`, including `path` itself.
pub fn closure(path: &Path) -> Result<Vec<PathBuf>> {
    let out = run("nix", &["path-info", "-r", path_str(path)?])?;
    Ok(lines_to_paths(&out))
}

/// Direct references of `path`, excluding self-references. Goes through
/// `nix path-info --json`'s `references` array — there's no subcommand that
/// lists just the direct references on its own.
pub fn references(path: &Path) -> Result<Vec<PathBuf>> {
    let out = run("nix", &["path-info", "--json", path_str(path)?])?;
    let parsed: serde_json::Value =
        serde_json::from_str(&out).context("nix path-info --json did not produce valid JSON")?;
    let entry = parsed
        .get(path_str(path)?)
        .with_context(|| format!("nix path-info --json had no entry for {}", path.display()))?;
    let refs = entry
        .get("references")
        .and_then(|r| r.as_array())
        .with_context(|| {
            format!(
                "nix path-info --json entry for {} has no references array",
                path.display()
            )
        })?;
    Ok(refs
        .iter()
        .filter_map(|v| v.as_str())
        .map(PathBuf::from)
        .filter(|p| p != path)
        .collect())
}

fn lines_to_paths(out: &str) -> Vec<PathBuf> {
    out.lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

/// `nix store dump-path <path>`: serialize a store path to a NAR byte stream.
pub fn dump(path: &Path) -> Result<Vec<u8>> {
    log::v(format!("running: nix store dump-path {}", path.display()));
    let output = Command::new("nix")
        .args(["store", "dump-path"])
        .arg(path)
        .output()
        .with_context(|| format!("failed to dump {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "nix store dump-path {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

/// `nix-store --restore <target> < nar`: unpack a NAR to `target`, which
/// must not already exist. Still legacy `nix-store` — the modern `nix
/// store` subcommands have no equivalent for unpacking to an arbitrary
/// non-store directory.
pub fn restore(nar: &[u8], target: &Path) -> Result<()> {
    use std::io::Write;
    log::v(format!(
        "running: nix-store --restore {} ({} NAR bytes)",
        target.display(),
        nar.len()
    ));
    let mut child = Command::new("nix-store")
        .arg("--restore")
        .arg(target)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn nix-store --restore {}", target.display()))?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(nar)
        .context("failed to write NAR to nix-store --restore")?;
    let status = child.wait().context("nix-store --restore did not exit")?;
    if !status.success() {
        bail!("nix-store --restore {} failed", target.display());
    }
    Ok(())
}

/// `nix store add --mode nar --hash-algo sha256 <dir>`: content-address
/// `dir` into the store, returning the resulting store path.
pub fn add_fixed_recursive(dir: &Path) -> Result<PathBuf> {
    let out = run(
        "nix",
        &[
            "store",
            "add",
            "--mode",
            "nar",
            "--hash-algo",
            "sha256",
            path_str(dir)?,
        ],
    )?;
    let line = out
        .lines()
        .next()
        .with_context(|| "nix store add produced no output")?;
    Ok(PathBuf::from(line))
}
