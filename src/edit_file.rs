use crate::{editor, log, store};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Dump `path` (which must be in `closure`), open `$EDITOR` on `subpath`
/// inside it (or the whole extracted tree if omitted), re-add the edited
/// tree as a fresh content-addressed store path, and return the
/// `(old, new)` pair. Grafting it up through the closure is the caller's
/// job (`main.rs`'s `collect_pairs`) — several of these, and/or
/// `--replace`/`--edit-drv`/`--edit-nix`, can be combined into one closure
/// walk, so producing the pair is kept separate from applying it.
pub fn produce_pair(closure_root: &Path, closure: &[PathBuf], path: &Path, subpath: Option<&Path>) -> Result<(PathBuf, PathBuf)> {
    store::require_output_path(path)?;
    if !closure.iter().any(|p| p == path) {
        bail!("{} is not in the closure of {}", path.display(), closure_root.display());
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
    Ok((path.to_path_buf(), new_path))
}
