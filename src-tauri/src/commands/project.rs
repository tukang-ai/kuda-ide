use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use crate::error::{AppError, Result};
use crate::state::AppState;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProjectInfo {
    pub root: PathBuf,
    pub name: String,
    pub app_data_dir: PathBuf,
}

#[tauri::command]
pub fn project_open(app: AppHandle, state: State<'_, AppState>, path: String) -> Result<ProjectInfo> {
    let root = PathBuf::from(&path)
        .canonicalize()
        .map_err(|e| AppError::General(format!("Cannot open folder '{}': {}", path, e)))?;

    if !root.is_dir() {
        return Err(AppError::General(format!("'{}' is not a directory", path)));
    }

    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::General(format!("Failed to resolve app data dir: {}", e)))?;
    std::fs::create_dir_all(&app_data_dir)?;

    state.set_project_root(root.clone());
    state.set_app_data_dir(app_data_dir.clone());

    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| root.to_string_lossy().to_string());

    tracing::info!("Opened project '{}' at {:?}", name, root);

    Ok(ProjectInfo {
        root,
        name,
        app_data_dir,
    })
}

#[tauri::command]
pub fn project_current(state: State<'_, AppState>) -> Option<String> {
    state.get_project_root().map(|p| p.to_string_lossy().to_string())
}

#[tauri::command]
pub fn open_external_url(url: String) -> std::result::Result<(), String> {
    // Only http/https URLs may be handed to the OS "open" command. Without this
    // check a compromised hub response (or a buggy frontend) could pass
    // `file:///...`, `x-apple.systempreferences:...` or any custom scheme and
    // launch arbitrary local apps.
    let trimmed = url.trim();
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err("Refusing to open non-http(s) URL".to_string());
    }

    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&url).spawn();

    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/C", "start", &url]).spawn();

    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(&url).spawn();

    Ok(())
}
