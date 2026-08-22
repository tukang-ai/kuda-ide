use std::fs;
use std::path::{Path, PathBuf};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use crate::error::{AppError, Result};
use crate::security::PathGuard;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileCheckpoint {
    pub checkpoint_id: String,
    pub original_file_path: PathBuf,
    pub backup_file_path: PathBuf,
    pub original_sha256: String,
    pub timestamp: DateTime<Utc>,
    pub agent_message_id: Option<String>,
    /// Id of the agent run / edit session that produced this checkpoint.
    /// Checkpoints sharing the same id are reverted together.
    #[serde(default)]
    pub session_id: Option<String>,
    /// False when the file did not exist at checkpoint time (i.e. it was
    /// created during the session). Reverting such a checkpoint deletes the file.
    #[serde(default = "default_true")]
    pub existed_before: bool,
}

fn default_true() -> bool {
    true
}

/// A group of checkpoints created by one agent run / edit session.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionSummary {
    pub session_id: String,
    pub timestamp: DateTime<Utc>,
    pub file_count: usize,
    pub files: Vec<PathBuf>,
}

pub struct CheckpointManager {
    history_root_dir: PathBuf,
}

impl CheckpointManager {
    pub fn new(app_data_dir: &Path) -> Result<Self> {
        let history_root_dir = app_data_dir.join("history");
        if !history_root_dir.exists() {
            fs::create_dir_all(&history_root_dir)?;
        }
        Ok(Self { history_root_dir })
    }

    /// Creates a FULL FILE CONTENT SNAPSHOT of target_file before modification.
    /// Copies 100% of original file content to a .bak file in App Data Dir.
    pub fn create_checkpoint(
        &self,
        target_file: &Path,
        project_root: &Path,
        agent_message_id: Option<String>,
    ) -> Result<FileCheckpoint> {
        // 1. Validate path security
        let canonical_file = PathGuard::validate_path_in_scope(target_file, project_root)?;
        self.create_checkpoint_in_session(&canonical_file, agent_message_id, None)
    }

    /// Creates a checkpoint from an already-canonicalized path (no scope check),
    /// tagged with an optional edit-session id. The file must already exist.
    pub fn create_checkpoint_in_session(
        &self,
        canonical_file: &Path,
        agent_message_id: Option<String>,
        session_id: Option<String>,
    ) -> Result<FileCheckpoint> {
        // 2. Read full original content
        let content_bytes = fs::read(canonical_file)?;
        let sha256_hash = sha256::digest(&content_bytes);

        // 3. Generate unique checkpoint ID and backup filename
        let checkpoint_id = Uuid::new_v4().to_string();
        let timestamp = Utc::now();
        let safe_filename = format!(
            "{}_{}_{}.bak",
            canonical_file.file_name().unwrap_or_default().to_string_lossy(),
            timestamp.format("%Y%m%d_%H%M%S_%f"),
            &checkpoint_id[..8]
        );

        let file_history_dir = self.history_root_dir.join(&sha256_hash[..16]);
        fs::create_dir_all(&file_history_dir)?;

        let backup_file_path = file_history_dir.join(&safe_filename);

        // 4. Save FULL FILE CONTENT to backup location
        fs::write(&backup_file_path, &content_bytes)?;

        let checkpoint = FileCheckpoint {
            checkpoint_id,
            original_file_path: canonical_file.to_path_buf(),
            backup_file_path,
            original_sha256: sha256_hash,
            timestamp,
            agent_message_id,
            session_id,
            existed_before: true,
        };

        self.write_metadata(&checkpoint)?;
        tracing::info!(
            "Full File Checkpoint created: {} -> {:?}",
            checkpoint.checkpoint_id,
            checkpoint.backup_file_path
        );

        Ok(checkpoint)
    }

    /// Records that a file was CREATED during a session (it did not exist when
    /// the snapshot was taken). No backup content is stored; reverting the
    /// session removes the file.
    pub fn create_new_file_checkpoint(
        &self,
        canonical_file: &Path,
        session_id: Option<String>,
    ) -> Result<FileCheckpoint> {
        let checkpoint_id = Uuid::new_v4().to_string();
        let timestamp = Utc::now();

        let checkpoint = FileCheckpoint {
            checkpoint_id,
            original_file_path: canonical_file.to_path_buf(),
            backup_file_path: PathBuf::new(),
            original_sha256: String::new(),
            timestamp,
            agent_message_id: None,
            session_id,
            existed_before: false,
        };

        self.write_metadata(&checkpoint)?;
        tracing::info!(
            "New-file checkpoint recorded (created in session): {:?}",
            canonical_file
        );
        Ok(checkpoint)
    }

    fn write_metadata(&self, checkpoint: &FileCheckpoint) -> Result<()> {
        let dir = match checkpoint.original_file_path.parent() {
            Some(p) => p,
            None => self.history_root_dir.as_path(),
        };
        let _ = dir; // metadata lives under the file's hash dir (or a fallback dir for new files)
        let meta_dir = if checkpoint.original_sha256.is_empty() {
            self.history_root_dir.join("new_files")
        } else {
            self.history_root_dir.join(&checkpoint.original_sha256[..16])
        };
        fs::create_dir_all(&meta_dir)?;
        let meta_path = meta_dir.join(format!("{}.json", checkpoint.checkpoint_id));
        let meta_json = serde_json::to_string_pretty(checkpoint)?;
        fs::write(&meta_path, meta_json)?;
        Ok(())
    }

    /// Lists all checkpoints stored in the history root, newest first.
    pub fn list_checkpoints(&self) -> Result<Vec<FileCheckpoint>> {
        let mut checkpoints = Vec::new();

        if self.history_root_dir.exists() {
            for dir_entry in fs::read_dir(&self.history_root_dir)? {
                let dir_entry = dir_entry?;
                let dir_path = dir_entry.path();
                if !dir_path.is_dir() {
                    continue;
                }
                for file_entry in fs::read_dir(&dir_path)? {
                    let file_entry = file_entry?;
                    let file_path = file_entry.path();
                    if file_path.extension().and_then(|e| e.to_str()) == Some("json") {
                        if let Ok(content) = fs::read_to_string(&file_path) {
                            if let Ok(checkpoint) = serde_json::from_str::<FileCheckpoint>(&content) {
                                checkpoints.push(checkpoint);
                            }
                        }
                    }
                }
            }
        }

        checkpoints.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        Ok(checkpoints)
    }

    /// Finds a checkpoint by ID.
    pub fn find_checkpoint(&self, checkpoint_id: &str) -> Result<FileCheckpoint> {
        self.list_checkpoints()?
            .into_iter()
            .find(|c| c.checkpoint_id == checkpoint_id)
            .ok_or_else(|| AppError::General(format!("Checkpoint {} not found", checkpoint_id)))
    }

    /// Groups checkpoints by edit-session id, newest session first.
    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let mut groups: Vec<SessionSummary> = Vec::new();
        for checkpoint in self.list_checkpoints()? {
            let Some(session_id) = checkpoint.session_id else { continue };
            let files = &mut groups.iter_mut().find(|g| g.session_id == session_id);
            if let Some(group) = files {
                group.file_count += 1;
                if !group.files.contains(&checkpoint.original_file_path) {
                    group.files.push(checkpoint.original_file_path.clone());
                }
                if checkpoint.timestamp < group.timestamp {
                    group.timestamp = checkpoint.timestamp;
                }
            } else {
                groups.push(SessionSummary {
                    session_id,
                    timestamp: checkpoint.timestamp,
                    file_count: 1,
                    files: vec![checkpoint.original_file_path.clone()],
                });
            }
        }
        for group in groups.iter_mut() {
            group.files.sort();
        }
        groups.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        Ok(groups)
    }

    /// All checkpoints belonging to one edit session.
    fn checkpoints_for_session(&self, session_id: &str) -> Result<Vec<FileCheckpoint>> {
        Ok(self
            .list_checkpoints()?
            .into_iter()
            .filter(|c| c.session_id.as_deref() == Some(session_id))
            .collect())
    }

    /// Reverts every file touched by an edit session back to its full state
    /// before the session ran:
    /// - files that existed before -> restored from their first snapshot
    /// - files created during the session -> deleted
    /// - files deleted during the session -> restored from the pre-delete snapshot
    pub fn revert_session(&self, session_id: &str, project_root: &Path) -> Result<Vec<PathBuf>> {
        let checkpoints = self.checkpoints_for_session(session_id)?;
        if checkpoints.is_empty() {
            // A run that never edited a file produces no checkpoints. There is
            // nothing to revert, so return an empty result instead of an error —
            // the UI surfaces this as a graceful "nothing to revert" notice.
            return Ok(Vec::new());
        }

        // Keep the EARLIEST checkpoint per file so we return to pre-session state.
        let mut by_file: Vec<FileCheckpoint> = Vec::new();
        for checkpoint in checkpoints {
            if let Some(existing) = by_file
                .iter_mut()
                .find(|c| c.original_file_path == checkpoint.original_file_path)
            {
                if checkpoint.timestamp < existing.timestamp {
                    *existing = checkpoint;
                }
            } else {
                by_file.push(checkpoint);
            }
        }

        let mut restored = Vec::new();
        for checkpoint in by_file {
            let target_file =
                PathGuard::validate_path_in_scope(&checkpoint.original_file_path, project_root)?;

            // SAFETY NET: snapshot the CURRENT content before it is overwritten
            // or removed. Without this, reverting a session destroys any work
            // done after that session (manual edits or later runs)
            // irreversibly. The safety snapshot is a normal checkpoint tagged
            // "pre-revert", so the revert itself stays undoable.
            if target_file.exists() {
                self.create_checkpoint_in_session(
                    &target_file,
                    Some("pre-revert".to_string()),
                    None,
                )
                .map_err(|e| {
                    AppError::General(format!(
                        "Refusing to revert session '{}': cannot snapshot current state of {:?}: {}",
                        session_id, target_file, e
                    ))
                })?;
            }

            if !checkpoint.existed_before {
                // File was created during the session -> remove it.
                if target_file.exists() {
                    fs::remove_file(&target_file)?;
                    tracing::info!(
                        "Revert session {}: removed created file {:?}",
                        session_id,
                        target_file
                    );
                }
            } else {
                if !checkpoint.backup_file_path.exists() {
                    return Err(AppError::General(format!(
                        "Backup file does not exist: {:?}",
                        checkpoint.backup_file_path
                    )));
                }
                let backup_bytes = fs::read(&checkpoint.backup_file_path)?;
                if let Some(parent) = target_file.parent() {
                    if !parent.exists() {
                        fs::create_dir_all(parent)?;
                    }
                }
                // Unique staging name (UUID suffix): a fixed name would collide
                // when two restores of same-stem files (main.rs / main.ts) run
                // concurrently, corrupting whichever rename lands second.
                let tmp_path = target_file.with_extension(format!(
                    "tmp_restore_{}",
                    &Uuid::new_v4().simple().to_string()[..8]
                ));
                fs::write(&tmp_path, &backup_bytes)?;
                fs::rename(&tmp_path, &target_file)?;
                tracing::info!(
                    "Revert session {}: restored full file {:?}",
                    session_id,
                    target_file
                );
            }
            restored.push(target_file);
        }

        Ok(restored)
    }

    /// Restores a full file from checkpoint. Copies 100% of backup file over original file.
    pub fn restore_checkpoint(
        &self,
        checkpoint: &FileCheckpoint,
        project_root: &Path,
    ) -> Result<PathBuf> {
        // 1. Validate security boundary
        let target_file = PathGuard::validate_path_in_scope(&checkpoint.original_file_path, project_root)?;

        // SAFETY NET: snapshot the CURRENT content before it gets overwritten
        // or removed so restoring an older checkpoint never destroys newer
        // work irreversibly.
        if target_file.exists() {
            self.create_checkpoint_in_session(
                &target_file,
                Some("pre-restore".to_string()),
                None,
            )
            .map_err(|e| {
                AppError::General(format!(
                    "Refusing to restore checkpoint '{}': cannot snapshot current state of {:?}: {}",
                    checkpoint.checkpoint_id, target_file, e
                ))
            })?;
        }

        if !checkpoint.existed_before {
            // The checkpoint was taken for a newly created file -> remove it.
            if target_file.exists() {
                fs::remove_file(&target_file)?;
            }
            return Ok(target_file);
        }

        if !checkpoint.backup_file_path.exists() {
            return Err(AppError::General(format!(
                "Backup file does not exist: {:?}",
                checkpoint.backup_file_path
            )));
        }

        // 2. Read full backup content
        let backup_bytes = fs::read(&checkpoint.backup_file_path)?;

        // 3. Atomic overwrite of original file with full backup content,
        //    using a unique staging name (see revert_session).
        let tmp_path = target_file.with_extension(format!(
            "tmp_restore_{}",
            &Uuid::new_v4().simple().to_string()[..8]
        ));
        fs::write(&tmp_path, &backup_bytes)?;
        fs::rename(&tmp_path, &target_file)?;

        tracing::info!(
            "Restored FULL FILE checkpoint {} to {:?}",
            checkpoint.checkpoint_id,
            target_file
        );

        Ok(target_file)
    }
}

// Real SHA-256 digest (the previous implementation used std's DefaultHasher,
// a 64-bit non-cryptographic SipHash, and silently mislabeled it as sha256 —
// a collision-prone fingerprint for checkpoint backups).
mod sha256 {
    pub fn digest(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(data);
        format!("{:x}", hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_restore_full_file_checkpoint() {
        let temp_dir = std::env::temp_dir().join("kuda_chk_test");
        let app_data = temp_dir.join("app_data");
        let project_root = temp_dir.join("project");

        let _ = fs::create_dir_all(&project_root);
        let target_file = project_root.join("main.rs");
        let original_text = "fn main() {\n    println!(\"Original Code\");\n}\n";
        let _ = fs::write(&target_file, original_text);

        let manager = CheckpointManager::new(&app_data).unwrap();

        // 1. Create Checkpoint (Full file snapshot)
        let chk = manager.create_checkpoint(&target_file, &project_root, None).unwrap();
        assert!(chk.backup_file_path.exists());

        // 2. Corrupt/Modify target file (Simulate AI wrong edit)
        let wrong_text = "fn main() {\n    // WRONG EDIT BY AI\n}\n";
        let _ = fs::write(&target_file, wrong_text);
        assert_eq!(fs::read_to_string(&target_file).unwrap(), wrong_text);

        // 3. Restore Checkpoint (1-Click Rollback)
        let restored_path = manager.restore_checkpoint(&chk, &project_root).unwrap();
        assert_eq!(restored_path, target_file.canonicalize().unwrap());
        assert_eq!(fs::read_to_string(&target_file).unwrap(), original_text);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_revert_session_restores_all_touched_files() {
        let temp_dir = std::env::temp_dir().join("kuda_sess_test");
        let app_data = temp_dir.join("app_data");
        let project_root = temp_dir.join("project");
        let _ = fs::create_dir_all(&project_root);

        let existing = project_root.join("main.rs");
        fs::write(&existing, "original main").unwrap();

        let manager = CheckpointManager::new(&app_data).unwrap();
        let session = "session_abc".to_string();

        // 1. Modify existing file (snapshot created).
        let chk1 = manager
            .create_checkpoint_in_session(&existing, None, Some(session.clone()))
            .unwrap();
        assert!(chk1.existed_before);
        fs::write(&existing, "edited main").unwrap();

        // 2. Create a brand-new file during the session.
        let new_file = project_root.join("new.txt");
        let chk2 = manager
            .create_new_file_checkpoint(&new_file, Some(session.clone()))
            .unwrap();
        assert!(!chk2.existed_before);
        fs::write(&new_file, "hello").unwrap();

        // 3. A different session edits another file (must NOT be reverted).
        let other = project_root.join("other.rs");
        fs::write(&other, "other original").unwrap();
        manager
            .create_checkpoint_in_session(&other, None, Some("session_other".to_string()))
            .unwrap();
        fs::write(&other, "other edited").unwrap();

        // Sessions are grouped correctly.
        let sessions = manager.list_sessions().unwrap();
        assert!(sessions.iter().any(|s| s.session_id == session));
        let ours = sessions.iter().find(|s| s.session_id == session).unwrap();
        assert!(ours.files.contains(&existing) && ours.files.contains(&new_file));

        // 4. Revert the session.
        let restored = manager.revert_session(&session, &project_root).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(fs::read_to_string(&existing).unwrap(), "original main");
        assert!(!new_file.exists());
        assert_eq!(fs::read_to_string(&other).unwrap(), "other edited");

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_revert_session_with_no_checkpoints_returns_empty() {
        let temp_dir = std::env::temp_dir().join("kuda_noop_revert_test");
        let app_data = temp_dir.join("app_data");
        let project_root = temp_dir.join("project");
        let _ = fs::create_dir_all(&project_root);

        let manager = CheckpointManager::new(&app_data).unwrap();

        // A session id with no checkpoint (run that never edited files) must be
        // a graceful no-op, not an error.
        let restored = manager
            .revert_session("session_with_no_edits", &project_root)
            .unwrap();
        assert!(restored.is_empty());

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_revert_session_restores_deleted_file() {
        let temp_dir = std::env::temp_dir().join("kuda_del_test");
        let app_data = temp_dir.join("app_data");
        let project_root = temp_dir.join("project");
        let _ = fs::create_dir_all(&project_root);

        let target = project_root.join("gone.rs");
        fs::write(&target, "will be deleted").unwrap();

        let manager = CheckpointManager::new(&app_data).unwrap();
        let session = "session_del".to_string();

        manager
            .create_checkpoint_in_session(&target, None, Some(session.clone()))
            .unwrap();
        fs::remove_file(&target).unwrap();

        manager.revert_session(&session, &project_root).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "will be deleted");

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
