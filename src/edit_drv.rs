use crate::derivation::OutputTarget;
use crate::replace::{ReplaceOptions, ReplaceResult};
use crate::{derivation, editor, log, replace, store};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

/// Edit a derivation's JSON (env/builder/args), rebuild it, and graft the
/// result up through `closure_root`. `path` may be a `.drv` path or a plain
/// store output path — the latter's deriver is looked up automatically, and
/// which output to edit is unambiguous from `path` itself. For a bare `.drv`
/// with more than one output, `output` (`--output <name>`) disambiguates
/// which one; for a single-output `.drv`, it's optional.
pub fn run(closure_root: &Path, path: &Path, output: Option<&str>, opts: &ReplaceOptions) -> Result<ReplaceResult> {
    store::require_output_path(closure_root)?;
    // Fail fast, before the potentially expensive rebuild below.
    store::canonicalize(closure_root)?;
    let is_drv_path = path.extension().and_then(|e| e.to_str()) == Some("drv");
    let drv_path = derivation::resolve_deriver(path)?;
    log::v(format!("editing derivation {}", drv_path.display()));
    let shown = derivation::show(&drv_path)?;
    let target = if is_drv_path {
        output.map(OutputTarget::Name)
    } else {
        Some(OutputTarget::Path(path))
    };
    let (output_name, old_out, inner) = derivation::locate_output(&shown, &drv_path, target)?;
    log::v(format!("current output ({output_name}): {}", old_out.display()));

    let tmp = tempfile::NamedTempFile::new().context("failed to create scratch file")?;
    std::fs::write(tmp.path(), serde_json::to_string_pretty(&inner)?)?;
    log::v(format!("opening $EDITOR on {}", tmp.path().display()));
    editor::edit(tmp.path())?;
    let edited: Value = serde_json::from_str(&std::fs::read_to_string(tmp.path())?)
        .context("edited derivation JSON is not valid JSON")?;

    let new_drv = derivation::add_with_retry(edited)?;
    let new_out = derivation::realise(&new_drv, &output_name, opts.nix_args)?;
    log::v(format!("rebuilt leaf: {} -> {}", old_out.display(), new_out.display()));

    replace::replace(closure_root, &[(old_out, new_out)], opts)
}
