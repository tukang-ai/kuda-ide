use tauri::ipc::Channel;
use tauri::State;
use crate::error::{AppError, Result};
use crate::state::AppState;
use crate::terminal::pty_manager::TerminalOutputPayload;
use crate::commands::resolve_path;

#[tauri::command]
pub fn terminal_spawn(
    state: State<'_, AppState>,
    on_output: Channel<TerminalOutputPayload>,
    cwd: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
) -> Result<String> {
    let root = state.get_project_root().unwrap_or_else(|| {
        std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir())
    });
    let work_dir = match cwd {
        Some(c) if !c.is_empty() => {
            let resolved = resolve_path(&root, &c);
            // The requested cwd must stay inside the active project root —
            // `../..` or an absolute path must not launch a shell elsewhere.
            crate::security::PathGuard::validate_path_in_scope(&resolved, &root)
                .map_err(|e| AppError::General(format!("Invalid terminal cwd: {}", e)))?
        }
        _ => root,
    };
    let session_id = state.terminal.spawn_session(&work_dir, cols.unwrap_or(120), rows.unwrap_or(30), on_output)?;
    Ok(session_id)
}

#[tauri::command]
pub fn terminal_write(state: State<'_, AppState>, session_id: String, data: String) -> Result<()> {
    state.terminal.write_to_session(&session_id, &data)
}

#[tauri::command]
pub fn terminal_resize(state: State<'_, AppState>, session_id: String, cols: u16, rows: u16) -> Result<()> {
    if cols == 0 || rows == 0 {
        return Err(AppError::General("Invalid terminal size".to_string()));
    }
    state.terminal.resize_session(&session_id, cols, rows)
}

#[tauri::command]
pub fn terminal_kill(state: State<'_, AppState>, session_id: String) -> Result<()> {
    state.terminal.kill_session(&session_id)
}

/// Lists the ids of currently LIVE terminal sessions (reaping any shell that
/// exited on its own first). The frontend polls this to drop dead tabs and to
/// clean up after itself when the terminal panel is closed.
#[tauri::command]
pub fn terminal_list(state: State<'_, AppState>) -> Vec<String> {
    state.terminal.reap_dead_sessions();
    state
        .terminal
        .session_ids()
        .unwrap_or_default()
}

/// Kills every live terminal session (used when the terminal panel is closed
/// so no shell process outlives the UI).
#[tauri::command]
pub fn terminal_close_all(state: State<'_, AppState>) {
    state.terminal.kill_all();
}
