use crate::derivation::OutputTarget;
use crate::{derivation, editor, log};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Opens `$EDITOR` on `inner` (a derivation's JSON) and submits the result
/// via `nix derivation add`, returning the new `.drv` path — shared by
/// `produce_pair` and `produce_pairs_all_outputs`, since the edit session
/// itself (one recipe, however many outputs it has) is identical either way.
fn edit_and_resubmit(inner: Value) -> Result<PathBuf> {
    let tmp = tempfile::NamedTempFile::new().context("failed to create scratch file")?;
    std::fs::write(tmp.path(), serde_json::to_string_pretty(&inner)?)?;
    log::v(format!("opening $EDITOR on {}", tmp.path().display()));
    editor::edit(tmp.path())?;
    let edited: Value = serde_json::from_str(&std::fs::read_to_string(tmp.path())?)
        .context("edited derivation JSON is not valid JSON")?;
    derivation::add_with_retry(edited)
}

/// Edit a derivation's JSON (env/builder/args), rebuild it, and return the
/// `(old, new)` output pair — grafting it up through the closure is the
/// caller's job (see `edit_file::produce_pair`'s doc comment). `path` may
/// be a `.drv` path or a plain store output path — the latter's deriver is
/// looked up automatically, and which output to edit is unambiguous from
/// `path` itself. For a bare `.drv` with more than one output, `output`
/// disambiguates which one; for a single-output `.drv`, pass `None`.
pub fn produce_pair(
    path: &Path,
    output: Option<&str>,
    nix_args: &[String],
) -> Result<(PathBuf, PathBuf)> {
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
    log::v(format!(
        "current output ({output_name}): {}",
        old_out.display()
    ));

    let new_drv = edit_and_resubmit(inner)?;
    let new_out = derivation::realise(&new_drv, &output_name, nix_args)?;
    log::v(format!(
        "rebuilt leaf: {} -> {}",
        old_out.display(),
        new_out.display()
    ));

    Ok((old_out, new_out))
}

/// Like `produce_pair`, but edits the derivation once and returns a pair
/// for *every* one of its outputs instead of just one — nix's own `^*`
/// ("all outputs") selector, applied to editing: one edit session changes
/// the recipe for every output at once anyway, so there's no reason to
/// make the caller pick just one.
pub fn produce_pairs_all_outputs(
    path: &Path,
    nix_args: &[String],
) -> Result<Vec<(PathBuf, PathBuf)>> {
    let drv_path = derivation::resolve_deriver(path)?;
    log::v(format!(
        "editing derivation {} (all outputs)",
        drv_path.display()
    ));
    let shown = derivation::show(&drv_path)?;
    let old_outputs = derivation::all_outputs(&shown, &drv_path)?;
    let inner = derivation::inner_derivation(&shown, &drv_path)?.clone();

    let new_drv = edit_and_resubmit(inner)?;

    let targets: Vec<(PathBuf, String)> = old_outputs
        .iter()
        .map(|(name, _)| (new_drv.clone(), name.clone()))
        .collect();
    let built = derivation::build_many(&targets, nix_args)?;

    old_outputs
        .into_iter()
        .map(|(name, old_path)| {
            let new_path = built
                .get(&(new_drv.clone(), name.clone()))
                .cloned()
                .with_context(|| {
                    format!(
                        "nix build did not report an output named `{name}` for {}",
                        new_drv.display()
                    )
                })?;
            log::v(format!(
                "rebuilt leaf ({name}): {} -> {}",
                old_path.display(),
                new_path.display()
            ));
            Ok((old_path, new_path))
        })
        .collect()
}
