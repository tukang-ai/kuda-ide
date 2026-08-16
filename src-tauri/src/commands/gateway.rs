use tauri::State;

use crate::error::Result;
use crate::gateway::rate_limiter::DailyUsage;
use crate::state::AppState;

#[tauri::command]
pub fn gateway_issue_token(state: State<'_, AppState>) -> Result<String> {
    state.gateway.issue_token()
}

#[tauri::command]
pub fn gateway_get_device_hash(state: State<'_, AppState>) -> Result<String> {
    state.gateway.get_device_hash()
}

#[tauri::command]
pub fn gateway_get_usage_stats(state: State<'_, AppState>) -> Result<Option<DailyUsage>> {
    Ok(state.gateway.get_usage_stats())
}
