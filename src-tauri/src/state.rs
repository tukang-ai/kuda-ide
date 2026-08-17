use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use crate::agent::tool_registry::{
    CancelFlag, DirectionDecisionRegistry, ExternalRequestRegistry, PlanDecisionRegistry,
    ToolRegistry,
};
use crate::terminal::multiplexer::TerminalMultiplexer;

/// The app-wide gateway handle. `AppState::new` installs it here so provider
/// resolution in `roles.rs` can wrap every streamed request in the security
/// pipeline regardless of which entry command started the run.
static GLOBAL_GATEWAY: std::sync::OnceLock<
    Arc<crate::gateway::gateway_hub::GatewayHub>,
> = std::sync::OnceLock::new();

/// Returns the global gateway handle (None in unit tests / before AppState).
pub fn global_gateway() -> Option<Arc<crate::gateway::gateway_hub::GatewayHub>> {
    GLOBAL_GATEWAY.get().cloned()
}

pub struct AppState {
    pub active_project_root: Mutex<Option<PathBuf>>,
    pub app_data_dir: Mutex<Option<PathBuf>>,
    pub terminal: Arc<TerminalMultiplexer>,
    pub tool_registry: Arc<ToolRegistry>,
    pub fs_watcher: Mutex<Option<crate::file_system::watcher::FileWatcher>>,
    pub gateway: Arc<crate::gateway::gateway_hub::GatewayHub>,
    pub external_requests: Arc<ExternalRequestRegistry>,
    /// Plan-approval gate requests (swarm human-in-the-loop), keyed by
    /// `request_id`; resolved by the `agent_resolve_plan_decision` command.
    pub plan_decisions: Arc<PlanDecisionRegistry>,
    /// Thinker-direction checkpoint requests (temp conclusion review before
    /// the full plan), keyed by `request_id`; resolved by the
    /// `agent_resolve_direction_decision` command.
    pub direction_decisions: Arc<DirectionDecisionRegistry>,
    /// Live cancellation flags for running agent runs, keyed by run id so the
    /// `agent_cancel_run` command can stop a specific run. Multiple concurrent
    /// runs may share one run id (a hostile or buggy frontend can send the
    /// same id twice), so each bucket holds ALL of that id's flags — a new run
    /// never overwrites a sibling run's cancel handle.
    pub active_runs: Mutex<HashMap<String, Vec<CancelFlag>>>,
    /// Loopback HTTP server untuk OAuth handoff (di-spin-up saat GitHub login
    /// dimulai, di-shutdown setelah selesai). `None` = tidak aktif.
    pub auth_loopback: tokio::sync::Mutex<Option<crate::gateway::auth_loopback::LoopbackServer>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        let gateway = Arc::new(crate::gateway::gateway_hub::GatewayHub::new());
        let _ = GLOBAL_GATEWAY.set(gateway.clone());
        Self {
            active_project_root: Mutex::new(None),
            app_data_dir: Mutex::new(None),
            terminal: Arc::new(TerminalMultiplexer::new()),
            tool_registry: Arc::new(ToolRegistry::new()),
            fs_watcher: Mutex::new(None),
            // No hardcoded secret: GatewayHub derives a random per-process key.
            gateway,
            external_requests: Arc::new(ExternalRequestRegistry::new()),
            plan_decisions: Arc::new(PlanDecisionRegistry::new()),
            direction_decisions: Arc::new(DirectionDecisionRegistry::new()),
            active_runs: Mutex::new(HashMap::new()),
            auth_loopback: tokio::sync::Mutex::new(None),
        }
    }

    pub fn set_fs_watcher(&self, watcher: crate::file_system::watcher::FileWatcher) {
        let mut slot = self.fs_watcher.lock().unwrap();
        *slot = Some(watcher);
    }

    pub fn set_project_root(&self, path: PathBuf) {
        let mut root = self.active_project_root.lock().unwrap();
        *root = Some(path);
    }

    pub fn get_project_root(&self) -> Option<PathBuf> {
        let root = self.active_project_root.lock().unwrap();
        root.clone()
    }

    pub fn set_app_data_dir(&self, path: PathBuf) {
        let mut dir = self.app_data_dir.lock().unwrap();
        *dir = Some(path);
    }

    pub fn get_app_data_dir(&self) -> Option<PathBuf> {
        let dir = self.app_data_dir.lock().unwrap();
        dir.clone()
    }

    pub fn require_project_root(&self) -> crate::error::Result<PathBuf> {
        self.get_project_root().ok_or_else(|| {
            crate::error::AppError::General("No active project. Open a folder first.".to_string())
        })
    }

    pub fn require_app_data_dir(&self) -> crate::error::Result<PathBuf> {
        self.get_app_data_dir().ok_or_else(|| {
            crate::error::AppError::General("App data directory not initialized.".to_string())
        })
    }
}
