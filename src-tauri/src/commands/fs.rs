use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use tauri::State;
use crate::error::{AppError, Result};
use crate::file_system::io::{DirEntryItem, FileContentPayload, FileSystemIO};
use crate::diff_engine::history::CheckpointManager;
use crate::security::PathGuard;
use crate::state::AppState;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WriteFileResponse {
    pub path: PathBuf,
    pub checkpoint_id: Option<String>,
}

/// Resolves `target` against the active project root. Paths inside the root are
/// returned as-is; paths outside the root raise an interactive Allow/Deny
/// notification and only proceed when the user approves. Returns the canonical
/// path to operate on.
async fn resolve_in_scope(
    state: &State<'_, AppState>,
    target: &PathBuf,
    reason: &str,
    kind: &str,
) -> Result<PathBuf> {
    let root = state.require_project_root()?;
    match PathGuard::validate_path_in_scope(target, &root) {
        Ok(canonical) => Ok(canonical),
        Err(scope_err) => {
            let allowed = state
                .external_requests
                .request_approval(&target.display().to_string(), reason, kind)
                .await;
            if allowed {
                let canonical = PathGuard::canonicalize_unchecked(target)
                    .map_err(|e| AppError::Security(e))?;
                // Same broad-root guard as the RLM kernel allowlist: approving
                // `/`, the home directory or a system tree with one click would
                // expose the whole machine. Require a narrow path.
                if crate::agent::rlm_kernel::is_broad_root(&canonical) {
                    return Err(AppError::General(format!(
                        "Refusing overly broad path '{}': approve a narrower path (a specific \
                         file or directory) instead.",
                        canonical.display()
                    )));
                }
                Ok(canonical)
            } else {
                Err(scope_err.into())
            }
        }
    }
}

#[tauri::command]
pub async fn fs_list_dir(state: State<'_, AppState>, path: String) -> Result<Vec<DirEntryItem>> {
    let root = state.require_project_root()?;
    let target = if path.is_empty() { root.clone() } else { PathBuf::from(&path) };
    let canonical = resolve_in_scope(
        &state,
        &target,
        "Open directory outside the active project",
        "dir_scan",
    )
    .await?;
    FileSystemIO::list_dir_canonical(&canonical)
}

#[tauri::command]
pub async fn fs_read_file(
    state: State<'_, AppState>,
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<FileContentPayload> {
    let target = PathBuf::from(&path);
    let canonical = resolve_in_scope(
        &state,
        &target,
        "Open file outside the active project",
        "file_read",
    )
    .await?;
    FileSystemIO::read_file_canonical(&canonical, start_line, end_line)
}

#[tauri::command]
pub async fn fs_write_file(
    state: State<'_, AppState>,
    path: String,
    content: String,
    agent_message_id: Option<String>,
) -> Result<WriteFileResponse> {
    let app_data = state.require_app_data_dir()?;
    let checkpoint_mgr = CheckpointManager::new(&app_data)?;
    let target = PathBuf::from(&path);

    let canonical = resolve_in_scope(
        &state,
        &target,
        "Write file outside the active project",
        "file_write",
    )
    .await?;

    if let Some(parent) = canonical.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let checkpoint = FileSystemIO::write_file_canonical_in_session(
        &canonical,
        &content,
        &checkpoint_mgr,
        agent_message_id,
        None,
    )?;
    Ok(WriteFileResponse {
        path: canonical,
        checkpoint_id: checkpoint.map(|c| c.checkpoint_id),
    })
}

#[tauri::command]
pub async fn fs_delete(state: State<'_, AppState>, path: String) -> Result<()> {
    let app_data = state.require_app_data_dir()?;
    let checkpoint_mgr = CheckpointManager::new(&app_data)?;
    let target = PathBuf::from(&path);
    let canonical = resolve_in_scope(
        &state,
        &target,
        "Delete path outside the active project",
        "file_delete",
    )
    .await?;
    FileSystemIO::delete_to_trash_canonical_in_session(&canonical, &checkpoint_mgr, None)?;
    Ok(())
}

#[tauri::command]
pub async fn fs_create_dir(state: State<'_, AppState>, path: String) -> Result<()> {
    let target = PathBuf::from(&path);
    let canonical = resolve_in_scope(
        &state,
        &target,
        "Create directory outside the active project",
        "dir_create",
    )
    .await?;
    std::fs::create_dir_all(&canonical)?;
    Ok(())
}

#[tauri::command]
pub async fn fs_rename(state: State<'_, AppState>, from: String, to: String) -> Result<PathBuf> {
    let from_path = PathBuf::from(&from);
    let to_path = PathBuf::from(&to);
    let canonical_from = resolve_in_scope(
        &state,
        &from_path,
        "Move/rename source outside the active project",
        "file_rename",
    )
    .await?;
    let canonical_to = resolve_in_scope(
        &state,
        &to_path,
        "Move/rename destination outside the active project",
        "file_rename",
    )
    .await?;
    std::fs::rename(&canonical_from, &canonical_to)?;
    Ok(canonical_to)
}

/// Starts a recursive file watcher on the active project root, forwarding
/// create/modify/delete events to the frontend channel. This wires the
/// long-dead `FileWatcher` so external edits (other tools, git checkouts, the
/// agent's `run_command`) refresh open tabs live instead of only after a run.
#[tauri::command]
pub fn fs_watch_start(
    state: State<'_, AppState>,
    on_event: tauri::ipc::Channel<crate::file_system::watcher::FsEvent>,
) -> Result<()> {
    let root = state.require_project_root()?;
    let watcher = crate::file_system::watcher::FileWatcher::new(move |ev| {
        let _ = on_event.send(ev);
    })?;
    let mut watcher = watcher;
    watcher.watch(&root)?;
    state.set_fs_watcher(watcher);
    tracing::info!("File watcher started for {:?}", root);
    Ok(())
}
