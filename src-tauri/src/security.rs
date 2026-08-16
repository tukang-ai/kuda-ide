use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SecurityError {
    #[error("Path traversal detected or path outside active project root: {0}")]
    AccessDenied(String),
    #[error("Failed to canonicalize path: {0}")]
    CanonicalizationFailed(String),
}

pub struct PathGuard;

impl PathGuard {
    /// Canonicalizes `target` without enforcing a project-scope boundary.
    /// Existing paths are canonicalized directly; paths that do not exist yet
    /// (new files/dirs) are resolved against the nearest existing ancestor so
    /// the returned path is stable and symlink-free.
    pub fn canonicalize_unchecked(target: &Path) -> Result<PathBuf, SecurityError> {
        if target.exists() {
            return target.canonicalize().map_err(|e| {
                SecurityError::CanonicalizationFailed(format!("{}: {}", target.display(), e))
            });
        }

        // Walk up to the nearest existing ancestor, collecting missing components.
        let mut missing: Vec<std::ffi::OsString> = Vec::new();
        let mut ancestor = target;
        while !ancestor.exists() {
            let name = ancestor
                .file_name()
                .ok_or_else(|| SecurityError::AccessDenied("Invalid path (no file name)".into()))?;
            missing.push(name.to_os_string());
            ancestor = ancestor
                .parent()
                .ok_or_else(|| SecurityError::AccessDenied("Invalid parent path".into()))?;
        }

        let canonical_ancestor = ancestor.canonicalize().map_err(|e| {
            SecurityError::CanonicalizationFailed(format!("{}: {}", ancestor.display(), e))
        })?;

        let mut result = canonical_ancestor;
        for component in missing.iter().rev() {
            result.push(component);
        }
        Ok(result)
    }

    /// Canonicalizes `target_path` and verifies that it is strictly contained within `project_root`.
    /// Rejects any path traversal attempts (`..`, symlinks outside workspace).
    /// Relative paths are resolved against `project_root` before validation.
    pub fn validate_path_in_scope<P1: AsRef<Path>, P2: AsRef<Path>>(
        target_path: P1,
        project_root: P2,
    ) -> Result<PathBuf, SecurityError> {
        let target = target_path.as_ref();
        let root = project_root.as_ref();

        // 1. Canonicalize project root
        let canonical_root = root
            .canonicalize()
            .map_err(|e| SecurityError::CanonicalizationFailed(format!("{}: {}", root.display(), e)))?;

        // 2. Resolve relative paths against the project root, then canonicalize
        let resolved = if target.is_absolute() {
            target.to_path_buf()
        } else {
            canonical_root.join(target)
        };
        let canonical_target = Self::canonicalize_unchecked(&resolved)?;

        // 3. Verify prefix boundary
        if canonical_target.starts_with(&canonical_root) {
            Ok(canonical_target)
        } else {
            Err(SecurityError::AccessDenied(format!(
                "Path '{}' is outside active project boundary '{}'",
                canonical_target.display(),
                canonical_root.display()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_valid_path_inside_root() {
        let temp_dir = std::env::temp_dir().join("kuda_sec_test");
        let _ = fs::create_dir_all(&temp_dir);
        let sub_file = temp_dir.join("test.txt");
        let _ = fs::write(&sub_file, "hello");

        let validated = PathGuard::validate_path_in_scope(&sub_file, &temp_dir);
        assert!(validated.is_ok());

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_reject_traversal_outside_root() {
        let temp_dir = std::env::temp_dir().join("kuda_sec_test_root");
        let outside_dir = std::env::temp_dir().join("kuda_sec_test_outside");
        let _ = fs::create_dir_all(&temp_dir);
        let _ = fs::create_dir_all(&outside_dir);
        let outside_file = outside_dir.join("secret.txt");
        let _ = fs::write(&outside_file, "secret");

        let validated = PathGuard::validate_path_in_scope(&outside_file, &temp_dir);
        assert!(validated.is_err());

        let _ = fs::remove_dir_all(&temp_dir);
        let _ = fs::remove_dir_all(&outside_dir);
    }

    #[test]
    fn test_relative_path_resolved_against_root() {
        let temp_dir = std::env::temp_dir().join("kuda_sec_test_rel");
        let _ = fs::create_dir_all(&temp_dir);
        let sub = temp_dir.join("src");
        fs::create_dir_all(&sub).unwrap();
        let file = sub.join("lib.rs");
        fs::write(&file, "fn main() {}").unwrap();

        // Relative path must resolve against the project root, not CWD.
        let validated = PathGuard::validate_path_in_scope("src/lib.rs", &temp_dir);
        assert!(validated.is_ok());
        assert_eq!(validated.unwrap(), file.canonicalize().unwrap());

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
