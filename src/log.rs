use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn set_verbose(v: bool) {
    VERBOSE.store(v, Ordering::Relaxed);
}

/// Print `msg` to stderr, prefixed, if `--verbose` was passed — graft's own
/// narration, separate from `nix_args`, which controls `nix build`'s.
pub fn v(msg: impl AsRef<str>) {
    if VERBOSE.load(Ordering::Relaxed) {
        eprintln!("[graft] {}", msg.as_ref());
    }
}
