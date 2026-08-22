use std::path::PathBuf;
use tauri::State;
use crate::diff_engine::calculator::{DiffCalculator, DiffResult};
use crate::diff_engine::history::{CheckpointManager, FileCheckpoint, SessionSummary};
use crate::error::Result;
use crate::file_system::io::FileSystemIO;
use crate::state::AppState;
use crate::commands::resolve_path;

#[tauri::command]
pub fn history_list_checkpoints(state: State<'_, AppState>) -> Result<Vec<FileCheckpoint>> {
    let app_data_dir = state.require_app_data_dir()?;
    let mgr = CheckpointManager::new(&app_data_dir)?;
    mgr.list_checkpoints()
}

/// Groups checkpoints by agent run / edit session (newest first).
#[tauri::command]
pub fn history_list_sessions(state: State<'_, AppState>) -> Result<Vec<SessionSummary>> {
    let app_data_dir = state.require_app_data_dir()?;
    let mgr = CheckpointManager::new(&app_data_dir)?;
    mgr.list_sessions()
}

/// Reverts every file touched by one agent run / edit session back to its
/// state before the session: modified files are fully restored, created files
/// are removed, deleted files are restored. Returns both the reverted paths
/// and per-file failure reasons (a locked file no longer aborts the whole
/// revert mid-loop).
#[tauri::command]
pub fn history_revert_session(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<crate::diff_engine::history::RevertSessionResult> {
    let app_data_dir = state.require_app_data_dir()?;
    let project_root = state.require_project_root()?;
    let mgr = CheckpointManager::new(&app_data_dir)?;
    mgr.revert_session(&session_id, &project_root)
}

#[tauri::command]
pub fn history_restore_checkpoint(state: State<'_, AppState>, checkpoint_id: String) -> Result<PathBuf> {
    let app_data_dir = state.require_app_data_dir()?;
    let project_root = state.require_project_root()?;
    let mgr = CheckpointManager::new(&app_data_dir)?;
    let checkpoint = mgr.find_checkpoint(&checkpoint_id)?;
    mgr.restore_checkpoint(&checkpoint, &project_root)
}

#[tauri::command]
pub fn diff_compute(
    state: State<'_, AppState>,
    path: String,
    modified_content: String,
) -> Result<DiffResult> {
    let root = state.require_project_root()?;
    let target = resolve_path(&root, &path);
    let original = match FileSystemIO::read_file(&target, &root, None, None) {
        Ok(payload) => payload.content,
        Err(_) => String::new(),
    };
    Ok(DiffCalculator::compute_diff(target, &original, &modified_content))
}
