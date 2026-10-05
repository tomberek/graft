use crate::log;
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
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

/// Just the 32-character hash prefix of a store path basename, before the
/// separating hyphen and name-version suffix — the same width Nix's own
/// post-build reference scan matches (it never requires a name to
/// follow), narrower than [`basename`]'s full `hash-name-version` string.
pub fn store_hash(path: &Path) -> Result<String> {
    let base = basename(path)?;
    if base.len() <= HASH_LEN {
        bail!("store path basename too short to contain a hash: {}", base);
    }
    Ok(base[..HASH_LEN].to_string())
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

/// Direct references of every path in `paths`, batched into a single `nix
/// path-info --json` call instead of one per path — a closure walk doing
/// one `nix path-info` per member doesn't scale to a real closure
/// (thousands of paths); `nix path-info --json` already accepts multiple
/// installables at once, returning one JSON object keyed by path, so
/// there's no reason to call it more than once. Same self-reference
/// exclusion as a single-path lookup would have.
pub fn references_many(paths: &[PathBuf]) -> Result<HashMap<PathBuf, Vec<PathBuf>>> {
    if paths.is_empty() {
        return Ok(HashMap::new());
    }
    let path_strs: Vec<&str> = paths.iter().map(|p| path_str(p)).collect::<Result<_>>()?;
    let mut args = vec!["path-info", "--json"];
    args.extend(path_strs);
    let out = run("nix", &args)?;
    let parsed: serde_json::Value =
        serde_json::from_str(&out).context("nix path-info --json did not produce valid JSON")?;
    let obj = parsed
        .as_object()
        .context("nix path-info --json did not produce a JSON object")?;
    let mut result = HashMap::with_capacity(paths.len());
    for path in paths {
        let entry = obj
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
        let refs: Vec<PathBuf> = refs
            .iter()
            .filter_map(|v| v.as_str())
            .map(PathBuf::from)
            .filter(|p| p != path)
            .collect();
        result.insert(path.clone(), refs);
    }
    Ok(result)
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

/// Scans every file (and symlink target) under `dir` for `/nix/store/
/// <hash>-<name>` references embedded in its content — a raw byte scan,
/// not a UTF-8 one, since edited content can be binary — returning each
/// *existing* top-level store item found. Nix's own post-build reference
/// scan only ever registers references among a build's *declared* inputs,
/// never an unbounded scan of the whole store, so this is what lets
/// `edit_file`'s synthetic-derivation re-add (see its own doc comment)
/// declare the right `inputSrcs` for the daemon to actually find them in.
pub fn scan_references(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found = HashSet::new();
    scan_dir(dir, &mut found)?;
    let mut out: Vec<PathBuf> = found.into_iter().collect();
    out.sort();
    Ok(out)
}

fn scan_dir(dir: &Path, found: &mut HashSet<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("failed to read directory {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to stat {}", path.display()))?;
        if file_type.is_symlink() {
            if let Ok(target) = std::fs::read_link(&path) {
                scan_bytes(target.as_os_str().as_encoded_bytes(), found);
            }
        } else if file_type.is_dir() {
            scan_dir(&path, found)?;
        } else if file_type.is_file() {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            scan_bytes(&bytes, found);
        }
    }
    Ok(())
}

fn scan_bytes(bytes: &[u8], found: &mut HashSet<PathBuf>) {
    let needle = format!("{STORE_DIR}/").into_bytes();
    let mut start = 0;
    while start + needle.len() <= bytes.len() {
        let Some(offset) = bytes[start..]
            .windows(needle.len())
            .position(|w| w == needle)
        else {
            break;
        };
        let after = start + offset + needle.len();
        let mut end = after;
        while end < bytes.len() && is_path_byte(bytes[end]) {
            end += 1;
        }
        let token = &bytes[after..end];
        let basename_end = token.iter().position(|&b| b == b'/').unwrap_or(token.len());
        if basename_end > 0 {
            if let Ok(name) = std::str::from_utf8(&token[..basename_end]) {
                let candidate = PathBuf::from(format!("{STORE_DIR}/{name}"));
                if candidate.exists() {
                    found.insert(candidate);
                }
            }
        }
        start = after;
    }
}

fn is_path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'+' | b'/')
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
