pub mod agent;
pub mod commands;
pub mod diff_engine;
pub mod error;
pub mod file_system;
pub mod gateway;
pub mod indexer;
pub mod security;
pub mod state;
pub mod terminal;

use state::AppState;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new())
        .setup(|app| {
            tracing_subscriber::fmt()
                .with_env_filter("kuda_ide=debug,info")
                .with_target(false)
                .compact()
                .init();

            let state = app.state::<AppState>();
            if let Ok(dir) = app.path().app_data_dir() {
                let _ = std::fs::create_dir_all(&dir);
                state.gateway.init_audit(&dir);
                state.set_app_data_dir(dir);
            }

            tracing::info!("KudaIDE Rust Engine & 5-Layer Security Gateway initialized successfully.");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // Project
            commands::project::project_open,
            commands::project::project_current,
            commands::project::open_external_url,
            // FileSystem
            commands::fs::fs_list_dir,
            commands::fs::fs_read_file,
            commands::fs::fs_write_file,
            commands::fs::fs_delete,
            commands::fs::fs_create_dir,
            commands::fs::fs_rename,
            commands::fs::fs_watch_start,
            // Terminal PTY
            commands::terminal::terminal_spawn,
            commands::terminal::terminal_write,
            commands::terminal::terminal_resize,
            commands::terminal::terminal_kill,
            commands::terminal::terminal_list,
            commands::terminal::terminal_close_all,
            // Indexer
            commands::indexer::search_code,
            commands::indexer::search_replace,
            commands::indexer::parse_symbols,
            // Agent
            commands::agent::agent_chat,
            commands::agent::agent_swarm_chat,
            commands::agent::agent_resume_run,
            commands::agent::agent_get_config,
            commands::agent::agent_delete_config,
            commands::agent::agent_save_key,
            commands::agent::agent_has_key,
            commands::agent::agent_refresh_hub_session,
            commands::agent::agent_ensure_hub_session,
            commands::agent::agent_save_hub_credentials,
            commands::agent::agent_has_hub_credentials,
            commands::agent::agent_hub_account,
            commands::agent::agent_hub_sign_out,
            commands::agent::auth_start_loopback,
            commands::agent::auth_stop_loopback,
            commands::agent::provider_list,
            commands::agent::provider_save,
            commands::agent::provider_delete,
            commands::agent::agent_config_get,
            commands::agent::agent_config_set,
            commands::agent::chat_list_sessions,
            commands::agent::chat_load_session,
            commands::agent::chat_delete_session,
            commands::agent::agent_approve_external_access,
            commands::agent::agent_deny_external_access,
            commands::agent::agent_resolve_plan_decision,
            commands::agent::agent_resolve_direction_decision,
            commands::agent::agent_bind_external_events,
            commands::agent::agent_cancel_run,
            // Gateway Commands
            commands::gateway::gateway_issue_token,
            commands::gateway::gateway_get_device_hash,
            commands::gateway::gateway_get_usage_stats,
            // History & Diff
            commands::history::history_list_checkpoints,
            commands::history::history_list_sessions,
            commands::history::history_revert_session,
            commands::history::history_restore_checkpoint,
            commands::history::diff_compute,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
