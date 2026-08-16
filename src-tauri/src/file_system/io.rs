use std::fs;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use crate::error::{AppError, Result};
use crate::security::PathGuard;
use crate::diff_engine::history::{CheckpointManager, FileCheckpoint};#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileContentPayload {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DirEntryItem {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

pub struct FileSystemIO;

impl FileSystemIO {
    /// Reads file content after verifying path scope against project_root.
    /// Supports optional start_line and end_line (1-indexed, inclusive).
    /// If start_line and end_line are None, reads 100% FULL FILE content.
    pub fn read_file(
        path: &Path,
        project_root: &Path,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<FileContentPayload> {
        let canonical_path = PathGuard::validate_path_in_scope(path, project_root)?;
        Self::read_file_canonical(&canonical_path, start_line, end_line)
    }

    /// Reads file content from an already-canonicalized path (no scope check).
    pub fn read_file_canonical(
        canonical_path: &Path,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<FileContentPayload> {
        let full_content = fs::read_to_string(canonical_path)?;

        let content = if start_line.is_some() || end_line.is_some() {
            let lines: Vec<&str> = full_content.lines().collect();
            let total_lines = lines.len();
            let start = start_line.unwrap_or(1).saturating_sub(1);
            let end = end_line.unwrap_or(total_lines).min(total_lines);

            if start >= total_lines || start >= end {
                String::new()
            } else {
                lines[start..end].join("\n")
            }
        } else {
            full_content
        };

        Ok(FileContentPayload {
            path: canonical_path.to_path_buf(),
            content,
        })
    }

    /// Safely writes content to target file:
    /// 1. Validates path security
    /// 2. Creates AUTOMATIC FULL FILE CHECKPOINT if file exists
    /// 3. Performs atomic write (.tmp -> rename)
    pub fn write_file(
        path: &Path,
        content: &str,
        project_root: &Path,
        checkpoint_mgr: &CheckpointManager,
        agent_message_id: Option<String>,
    ) -> Result<Option<FileCheckpoint>> {
        let canonical_path = PathGuard::validate_path_in_scope(path, project_root)?;
        Self::write_file_canonical(&canonical_path, content, checkpoint_mgr, agent_message_id)
    }

    /// Writes content to an already-canonicalized path (no scope check).
    pub fn write_file_canonical(
        canonical_path: &Path,
        content: &str,
        checkpoint_mgr: &CheckpointManager,
        agent_message_id: Option<String>,
    ) -> Result<Option<FileCheckpoint>> {
        // 1. Create Automatic Full File Checkpoint if existing
        let checkpoint = if canonical_path.exists() {
            Some(checkpoint_mgr.create_checkpoint_in_session(
                canonical_path,
                agent_message_id,
                None,
            )?)
        } else {
            None
        };

        // 2. Atomic write (tmp -> rename). The tmp name carries a unique suffix
        // so two concurrent writes to the same file (two agent runs, or an
        // agent + a manual save) never clobber each other's staging file — a
        // shared `foo.tmp_write` used to be overwritten mid-flight and produced
        // a stale/forked rename.
        let tmp_path = canonical_path.with_extension(format!(
            "tmp_write_{}",
            Uuid::new_v4().simple()
        ));
        fs::write(&tmp_path, content)?;
        fs::rename(&tmp_path, canonical_path)?;

        tracing::info!("Successfully wrote file: {:?}", canonical_path);
        Ok(checkpoint)
    }

    /// Session-aware write: snapshots the pre-write full file (or records the
    /// file as newly created) tagged with `session_id`, then writes atomically.
    pub fn write_file_in_session(
        path: &Path,
        content: &str,
        project_root: &Path,
        checkpoint_mgr: &CheckpointManager,
        agent_message_id: Option<String>,
        session_id: Option<String>,
    ) -> Result<Option<FileCheckpoint>> {
        let canonical_path = PathGuard::validate_path_in_scope(path, project_root)?;
        Self::write_file_canonical_in_session(
            &canonical_path,
            content,
            checkpoint_mgr,
            agent_message_id,
            session_id,
        )
    }

    /// Session-aware write on an already-canonicalized path (no scope check).
    pub fn write_file_canonical_in_session(
        canonical_path: &Path,
        content: &str,
        checkpoint_mgr: &CheckpointManager,
        agent_message_id: Option<String>,
        session_id: Option<String>,
    ) -> Result<Option<FileCheckpoint>> {
        let checkpoint = if canonical_path.exists() {
            Some(checkpoint_mgr.create_checkpoint_in_session(
                canonical_path,
                agent_message_id,
                session_id,
            )?)
        } else {
            Some(checkpoint_mgr.create_new_file_checkpoint(canonical_path, session_id)?)
        };

        let tmp_path = canonical_path.with_extension(format!(
            "tmp_write_{}",
            Uuid::new_v4().simple()
        ));
        fs::write(&tmp_path, content)?;
        fs::rename(&tmp_path, canonical_path)?;

        tracing::info!("Successfully wrote file: {:?}", canonical_path);
        Ok(checkpoint)
    }

    /// Session-aware delete: snapshots the full file before moving it to Trash.
    pub fn delete_to_trash_in_session(
        path: &Path,
        project_root: &Path,
        checkpoint_mgr: &CheckpointManager,
        session_id: Option<String>,
    ) -> Result<Option<FileCheckpoint>> {
        let canonical_path = PathGuard::validate_path_in_scope(path, project_root)?;
        Self::delete_to_trash_canonical_in_session(&canonical_path, checkpoint_mgr, session_id)
    }

    /// Session-aware delete on an already-canonicalized path (no scope check).
    pub fn delete_to_trash_canonical_in_session(
        canonical_path: &Path,
        checkpoint_mgr: &CheckpointManager,
        session_id: Option<String>,
    ) -> Result<Option<FileCheckpoint>> {
        let checkpoint = if canonical_path.exists() {
            Some(checkpoint_mgr.create_checkpoint_in_session(
                canonical_path,
                None,
                session_id,
            )?)
        } else {
            None
        };
        trash::delete(canonical_path)
            .map_err(|e| AppError::General(format!("Failed to move to trash: {}", e)))?;
        tracing::info!("Moved to OS Trash: {:?}", canonical_path);
        Ok(checkpoint)
    }

    /// Deletes file or directory by moving to OS Trash via trash crate
    pub fn delete_to_trash(path: &Path, project_root: &Path) -> Result<()> {
        let canonical_path = PathGuard::validate_path_in_scope(path, project_root)?;
        Self::delete_to_trash_canonical(&canonical_path)
    }

    /// Moves an already-canonicalized path to the OS Trash (no scope check).
    pub fn delete_to_trash_canonical(canonical_path: &Path) -> Result<()> {
        trash::delete(canonical_path)
            .map_err(|e| AppError::General(format!("Failed to move to trash: {}", e)))?;
        tracing::info!("Moved to OS Trash: {:?}", canonical_path);
        Ok(())
    }

    /// Lists entries in directory
    pub fn list_dir(dir_path: &Path, project_root: &Path) -> Result<Vec<DirEntryItem>> {
        let canonical_dir = PathGuard::validate_path_in_scope(dir_path, project_root)?;
        Self::list_dir_canonical(&canonical_dir)
    }

    /// Lists entries in an already-canonicalized directory (no scope check).
    pub fn list_dir_canonical(canonical_dir: &Path) -> Result<Vec<DirEntryItem>> {
        let mut items = Vec::new();

        for entry in fs::read_dir(canonical_dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type()?.is_dir();

            items.push(DirEntryItem { name, path, is_dir });
        }

        items.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_file_with_checkpoint() {
        let temp_dir = std::env::temp_dir().join("kuda_io_test");
        let app_data = temp_dir.join("app_data");
        let project_root = temp_dir.join("project");

        let _ = fs::create_dir_all(&project_root);
        let test_file = project_root.join("config.toml");
        let _ = fs::write(&test_file, "key = \"value1\"\n");

        let chk_mgr = CheckpointManager::new(&app_data).unwrap();

        // Write new content
        let chk = FileSystemIO::write_file(&test_file, "key = \"value2\"\n", &project_root, &chk_mgr, None).unwrap();
        assert!(chk.is_some());

        let read_back = FileSystemIO::read_file(&test_file, &project_root, None, None).unwrap();
        assert_eq!(read_back.content, "key = \"value2\"\n");

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
