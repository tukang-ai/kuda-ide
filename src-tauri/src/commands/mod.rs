pub mod agent;
pub mod fs;
pub mod gateway;
pub mod history;
pub mod indexer;
pub mod project;
pub mod terminal;

use std::path::{Path, PathBuf};

/// Normalizes a frontend-supplied path: relative paths are joined against the
/// active project root, absolute paths are used as-is. Subsequent PathGuard
/// validation guarantees the result stays inside the workspace boundary.
pub fn resolve_path(root: &Path, path: &str) -> PathBuf {
    let candidate = PathBuf::from(path);
    if candidate.is_absolute() {
        candidate
    } else {
        root.join(candidate)
    }
}
