use crate::{log, store};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Resolve `name` on `$PATH`, requiring a Nix store path (it must be
/// declarable as a derivation input). Doesn't follow symlinks: `nix-store`
/// is a symlink to `nix` that dispatches on argv[0], so resolving it away
/// would silently turn it into a plain, behaviorally different `nix` call.
pub fn tool_path(name: &str) -> Result<PathBuf> {
    log::v(format!("resolving tool path for `{name}`"));
    let found = Command::new("which")
        .arg(name)
        .output()
        .with_context(|| format!("failed to run `which {name}`"))?;
    if !found.status.success() {
        bail!("`{name}` not found on PATH");
    }
    let raw = PathBuf::from(String::from_utf8_lossy(&found.stdout).trim());
    if !raw.starts_with(store::STORE_DIR) {
        bail!(
            "`{name}` resolves to {}, which is not in the Nix store; graft needs \
             store-provided coreutils (see the `bash`/`gnused` packages in flake.nix's devShell)",
            raw.display()
        );
    }
    log::v(format!("`{name}` resolved to {}", raw.display()));
    Ok(raw)
}

pub fn current_system() -> Result<String> {
    log::v("running: nix eval --impure --raw --expr builtins.currentSystem");
    let out = Command::new("nix")
        .args(["eval", "--impure", "--raw", "--expr", "builtins.currentSystem"])
        .output()
        .context("failed to run `nix eval` for builtins.currentSystem")?;
    if !out.status.success() {
        bail!(
            "failed to determine current system: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A minimal single-output derivation spec, bypassing stdenv entirely.
pub struct DerivationSpec {
    pub name: String,
    pub builder: PathBuf,
    pub args: Vec<String>,
    /// Plain (non-derivation) store paths the sandbox must have access to.
    pub input_srcs: Vec<PathBuf>,
}

/// Build the ATerm-JSON body for `spec`, self-correct its output path via
/// [`add_with_retry`], and realise it. `nix_args` are forwarded verbatim to
/// the `nix build` call (e.g. `-Lv`, `--builders ...`).
pub fn build_and_realise(spec: DerivationSpec, nix_args: &[String]) -> Result<PathBuf> {
    log::v(format!(
        "constructing synthetic derivation `{}` with builder {} and {} input src(s)",
        spec.name,
        spec.builder.display(),
        spec.input_srcs.len()
    ));
    let system = current_system()?;
    let srcs = spec
        .input_srcs
        .iter()
        .map(|p| store::basename(p))
        .collect::<Result<Vec<_>>>()?;

    // A syntactically valid placeholder; add_with_retry corrects it below.
    let placeholder_basename = format!("{}-{}", "0".repeat(32), spec.name);
    let placeholder_path = format!("{}/{placeholder_basename}", store::STORE_DIR);

    let inner = serde_json::json!({
        "version": 4,
        "name": spec.name,
        "system": system,
        "builder": spec.builder.to_string_lossy(),
        "args": spec.args,
        "env": {
            "name": spec.name,
            "system": system,
            "builder": spec.builder.to_string_lossy(),
            "out": placeholder_path,
            "outputs": "out",
        },
        "outputs": { "out": { "path": placeholder_basename } },
        "inputs": { "drvs": {}, "srcs": srcs },
    });

    let new_drv = add_with_retry(inner)?;
    realise(&new_drv, "out", nix_args)
}

/// `nix derivation add` checks any output path / mirrored env var we supply
/// against its own computed hash (no public API exists for that hash) and
/// names the correct value in its error rather than computing it for us —
/// so: try, and on a recognized "should be" error, patch and retry.
pub fn add_with_retry(mut inner: Value) -> Result<PathBuf> {
    for attempt in 0..10 {
        log::v(format!("nix derivation add attempt {}", attempt + 1));
        match try_add(&inner) {
            Ok(path) => {
                log::v(format!("nix derivation add succeeded: {}", path.display()));
                return Ok(path);
            }
            Err(stderr) => match parse_correction(&stderr) {
                Some(corr) => {
                    log::v(format!("self-correcting: {}", corr.describe()));
                    apply_correction(&mut inner, &corr)?;
                }
                None => bail!("nix derivation add failed and the error was not auto-correctable:\n{stderr}"),
            },
        }
    }
    bail!("nix derivation add did not converge after repeated auto-corrections")
}

fn try_add(inner: &Value) -> Result<PathBuf, String> {
    let mut child = Command::new("nix")
        .args(["derivation", "add"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(serde_json::to_string(inner).map_err(|e| e.to_string())?.as_bytes())
        .map_err(|e| e.to_string())?;
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if output.status.success() {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .map(|l| PathBuf::from(l.trim()))
            .ok_or_else(|| "nix derivation add produced no output".to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

enum Correction {
    Output { incorrect: String, correct: String },
    EnvVar { name: String, correct: String },
}

impl Correction {
    fn describe(&self) -> String {
        match self {
            Correction::Output { incorrect, correct } => format!("output path {incorrect} -> {correct}"),
            Correction::EnvVar { name, correct } => format!("env.{name} -> {correct}"),
        }
    }
}

fn parse_correction(stderr: &str) -> Option<Correction> {
    // Anchor on the error phrase first — nix may emit unrelated quoted
    // `warning: ...` lines before it.
    if let Some(idx) = stderr.find("incorrect output") {
        let q = quoted_strings(&stderr[idx..]);
        if q.len() >= 2 {
            return Some(Correction::Output {
                incorrect: q[0].to_string(),
                correct: q[1].to_string(),
            });
        }
    }
    if let Some(idx) = stderr.find("incorrect environment variable") {
        let q = quoted_strings(&stderr[idx..]);
        if q.len() >= 2 {
            return Some(Correction::EnvVar {
                name: q[0].to_string(),
                correct: q[1].to_string(),
            });
        }
    }
    None
}

fn quoted_strings(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find('\'') {
        let after = &rest[start + 1..];
        match after.find('\'') {
            Some(end) => {
                out.push(&after[..end]);
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    out
}

fn apply_correction(inner: &mut Value, corr: &Correction) -> Result<()> {
    match corr {
        Correction::Output { incorrect, correct } => {
            let incorrect_base = basename_of(incorrect);
            let correct_base = basename_of(correct);
            let outputs = inner
                .get_mut("outputs")
                .and_then(|o| o.as_object_mut())
                .context("derivation has no `outputs` to correct")?;
            let mut found = false;
            for (_, v) in outputs.iter_mut() {
                let matches = v
                    .get("path")
                    .and_then(|p| p.as_str())
                    .map(|p| p == incorrect_base || p == incorrect.as_str())
                    .unwrap_or(false);
                if matches {
                    v["path"] = Value::String(correct_base.to_string());
                    found = true;
                }
            }
            if !found {
                bail!("could not find an output matching `{incorrect}` to correct");
            }
        }
        Correction::EnvVar { name, correct } => {
            let env = inner
                .get_mut("env")
                .and_then(|o| o.as_object_mut())
                .context("derivation has no `env` to correct")?;
            env.insert(name.clone(), Value::String(correct.clone()));
        }
    }
    Ok(())
}

fn basename_of(raw: &str) -> &str {
    raw.rsplit('/').next().unwrap_or(raw)
}

/// `nix path-info --derivation <path>`. Bails with a friendly message if
/// `path` has no known deriver (content directly `nix store add`ed, or a
/// previous graft) — there's no build recipe to rebuild in that case.
pub fn deriver_of(path: &Path) -> Result<PathBuf> {
    log::v(format!("running: nix path-info --derivation {}", path.display()));
    let output = Command::new("nix")
        .args(["path-info", "--derivation"])
        .arg(path)
        .output()
        .with_context(|| format!("failed to run nix path-info --derivation {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "{} has no known deriver, so there's no build recipe to rebuild — \
             try graft mode (the default) for this path instead",
            path.display()
        );
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    log::v(format!("{} deriver: {raw}", path.display()));
    Ok(PathBuf::from(raw))
}

/// Accept either a `.drv` path (used as-is) or a plain store output path
/// (resolved via [`deriver_of`]).
pub fn resolve_deriver(path: &Path) -> Result<PathBuf> {
    if path.extension().and_then(|e| e.to_str()) == Some("drv") {
        log::v(format!("{} is already a .drv path", path.display()));
        Ok(path.to_path_buf())
    } else {
        deriver_of(path)
    }
}

/// `nix derivation show <drv>`, which wraps its result as
/// `{"derivations": {"<basename>.drv": {...}}}`.
pub fn show(drv_path: &Path) -> Result<Value> {
    log::v(format!("running: nix derivation show {}", drv_path.display()));
    let output = Command::new("nix")
        .args(["derivation", "show"])
        .arg(drv_path)
        .output()
        .context("failed to run `nix derivation show`")?;
    if !output.status.success() {
        bail!(
            "nix derivation show {} failed: {}",
            drv_path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("nix derivation show {} did not produce valid JSON", drv_path.display()))
}

/// Which output of a (possibly multi-output) derivation to locate.
pub enum OutputTarget<'a> {
    /// Find whichever output produces this exact store path.
    Path(&'a Path),
    /// Find the output with this exact name (e.g. `"dev"`, `"man"`).
    Name(&'a str),
}

/// Extract the inner derivation object (what `nix derivation add` expects)
/// plus the resolved output name/path from `nix derivation show`'s wrapper.
/// `target = None` requires exactly one output; otherwise it's found by
/// path or by name, regardless of how many other outputs exist.
pub fn locate_output(shown: &Value, drv_path: &Path, target: Option<OutputTarget>) -> Result<(String, PathBuf, Value)> {
    let derivations = shown
        .get("derivations")
        .with_context(|| format!("unexpected `nix derivation show` schema for {}", drv_path.display()))?;
    let obj = derivations
        .as_object()
        .context("`derivations` field is not an object")?;
    let (_, inner) = obj
        .iter()
        .next()
        .with_context(|| format!("no derivation found for {}", drv_path.display()))?;
    let outputs = inner
        .get("outputs")
        .and_then(|o| o.as_object())
        .context("derivation has no `outputs`")?;

    let (name, raw_path) = match target {
        Some(OutputTarget::Path(p)) => {
            let target_base = store::basename(p)?;
            outputs
                .iter()
                .find_map(|(name, v)| {
                    let raw = v.get("path")?.as_str()?;
                    (raw == target_base).then(|| (name.clone(), raw.to_string()))
                })
                .with_context(|| {
                    format!(
                        "{} does not produce an output matching {} (its outputs: {})",
                        drv_path.display(),
                        p.display(),
                        output_names(outputs)
                    )
                })?
        }
        Some(OutputTarget::Name(n)) => {
            let v = outputs
                .get(n)
                .with_context(|| format!("{} has no output named `{n}` (its outputs: {})", drv_path.display(), output_names(outputs)))?;
            let raw = v.get("path").and_then(|p| p.as_str()).context("output has no `path`")?;
            (n.to_string(), raw.to_string())
        }
        None => {
            if outputs.len() != 1 {
                bail!(
                    "{} has {} outputs ({}) — pass a specific output path, or --output <name>, to disambiguate",
                    drv_path.display(),
                    outputs.len(),
                    output_names(outputs)
                );
            }
            let (name, v) = outputs.iter().next().unwrap();
            let raw = v.get("path").and_then(|p| p.as_str()).context("output has no `path`")?;
            (name.clone(), raw.to_string())
        }
    };
    Ok((name, full_store_path(&raw_path), inner.clone()))
}

fn output_names(outputs: &serde_json::Map<String, Value>) -> String {
    outputs.keys().cloned().collect::<Vec<_>>().join(", ")
}

fn full_store_path(raw: &str) -> PathBuf {
    if raw.starts_with('/') {
        PathBuf::from(raw)
    } else {
        PathBuf::from(format!("{}/{raw}", store::STORE_DIR))
    }
}

/// Builds `drv`'s `output_name` output via `nix build <drv>^<output_name>`.
/// `nix_args` are forwarded verbatim, e.g. `-Lv`, `--builders ssh://...`.
pub fn realise(drv: &Path, output_name: &str, nix_args: &[String]) -> Result<PathBuf> {
    nix_build(&[format!("{}^{output_name}", drv.display())], nix_args)
}

/// Runs `nix build <build_args...> --no-link --print-out-paths <nix_args...>`
/// and returns the first printed output path. Shared by [`realise`],
/// `edit_nix`'s attribute rebuild, and installable resolution — all three
/// are "ask `nix build` for an output path" with a different installable.
pub fn nix_build(build_args: &[String], nix_args: &[String]) -> Result<PathBuf> {
    let mut args: Vec<&str> = vec!["build"];
    args.extend(build_args.iter().map(String::as_str));
    args.push("--no-link");
    args.push("--print-out-paths");
    log::v(format!(
        "running: nix {}{}",
        args.join(" "),
        if nix_args.is_empty() { String::new() } else { format!(" {}", nix_args.join(" ")) }
    ));
    // Inherit stderr so build logs stream live; only --print-out-paths's
    // stdout (the result) needs capturing.
    let mut child = Command::new("nix")
        .args(&args)
        .args(nix_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to spawn nix {}", args.join(" ")))?;
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("piped stdout")
        .read_to_string(&mut stdout)
        .with_context(|| format!("failed to read output of nix {}", args.join(" ")))?;
    let status = child.wait().with_context(|| format!("nix {} did not exit", args.join(" ")))?;
    if !status.success() {
        bail!("nix {} failed (see build output above)", args.join(" "));
    }
    stdout
        .lines()
        .next()
        .map(|l| PathBuf::from(l.trim()))
        .with_context(|| format!("nix {} produced no output", args.join(" ")))
}

/// Creates (or replaces) a GC-root symlink at `link` pointing at `target`,
/// via `nix build <target> --out-link <link>` — a real registered root, not
/// just a plain symlink, and the same mechanism `nix build -o` itself uses.
/// Every build this tool does otherwise passes `--no-link`, so without this
/// nothing produced here is protected from a concurrent garbage collection.
pub fn add_out_link(target: &Path, link: &Path) -> Result<()> {
    log::v(format!("running: nix build {} --out-link {}", target.display(), link.display()));
    let status = Command::new("nix")
        .args(["build", &target.display().to_string(), "--out-link", &link.display().to_string()])
        .status()
        .with_context(|| format!("failed to spawn nix build {} --out-link {}", target.display(), link.display()))?;
    if !status.success() {
        bail!("nix build {} --out-link {} failed", target.display(), link.display());
    }
    Ok(())
}
