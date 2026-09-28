use crate::replace::{ReplaceOptions, ReplaceResult};
use crate::{editor, log, replace, store};
use anyhow::{bail, Context, Result};
use std::path::Path;

/// Dump `path` (which must be in `closure_root`'s closure), open `$EDITOR` on
/// `subpath` inside it (or the whole extracted tree if omitted), re-add the
/// edited tree as a fresh content-addressed store path, and graft it up
/// through `closure_root`.
pub fn run(
    closure_root: &Path,
    path: &Path,
    subpath: Option<&Path>,
    opts: &ReplaceOptions,
) -> Result<ReplaceResult> {
    store::require_output_path(closure_root)?;
    store::require_output_path(path)?;
    store::canonicalize(closure_root)?;
    let closure = store::closure(closure_root)?;
    if !closure.iter().any(|p| p == path) {
        bail!(
            "{} is not in the closure of {}",
            path.display(),
            closure_root.display()
        );
    }

    let name = store::store_name(path)?;
    log::v(format!("dumping {} to extract for editing", path.display()));
    let nar = store::dump(path)?;
    let tmp = tempfile::tempdir().context("failed to create scratch dir")?;
    let extracted = tmp.path().join(&name);
    store::restore(&nar, &extracted)?;
    log::v(format!("extracted to {}", extracted.display()));

    let edit_target = match subpath {
        Some(sp) => extracted.join(sp),
        None => extracted.clone(),
    };
    if !edit_target.exists() {
        bail!(
            "{} does not exist inside {}",
            edit_target.display(),
            path.display()
        );
    }
    log::v(format!("opening $EDITOR on {}", edit_target.display()));
    editor::edit(&edit_target)?;

    log::v(format!("re-adding {} to the store", extracted.display()));
    let new_path = store::add_fixed_recursive(&extracted)?;
    log::v(format!("edited path: {} -> {}", path.display(), new_path.display()));
    replace::replace(closure_root, &[(path.to_path_buf(), new_path)], opts)
}
