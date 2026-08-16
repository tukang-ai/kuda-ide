use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;
use crate::error::{AppError, Result};

/// Base URL server Kuda Hub (publik via Cloudflare Tunnel).
/// Satu-satunya sumber kebenaran; semua harga/plans/model diambil hub API.
pub const HUB_BASE_URL: &str = "https://kuda-ide.my.id/api/v1";

/// A configured LLM provider. Each provider owns its own API key (kept in the OS
/// Keychain under `provider_key.<id>`) and lists the model names it exposes.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub models: Vec<String>,
}

impl Provider {
    pub fn keychain_service(&self) -> String {
        format!("provider_key.{}", self.id)
    }
}

/// A role-to-model binding: which provider + which model a role (or a reviewer slot) uses.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ModelRef {
    #[serde(default)]
    pub provider_id: String,
    #[serde(default)]
    pub model: String,
}

/// Agent role assignment. Reviewer is a list so multiple reviewers are supported.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentConfig {
    #[serde(default)]
    pub thinker: ModelRef,
    #[serde(default)]
    pub reviewers: Vec<ModelRef>,
    /// Planning Writer: a CHEAP model that drafts the FULL detailed plan from
    /// the Thinker's approved direction + validated brief. The (expensive)
    /// Thinker only READS the draft and emits a short approve/revise decision,
    /// so the costly plan-writing output stays on the cheap model. Their review
    /// loop runs in a PRIVATE context that never enters the shared swarm history.
    #[serde(default)]
    pub planning_writer: ModelRef,
    #[serde(default)]
    pub executor_code: ModelRef,
    #[serde(default)]
    pub executor_design: ModelRef,
    #[serde(default)]
    pub executor_reviewer: ModelRef,
    #[serde(default)]
    pub rlm_model: ModelRef,
    #[serde(default)]
    pub rlm_verifier: ModelRef,
    /// Plan Approval Gate: when enabled, the swarm pauses after the Thinker's
    /// plan and waits for the user to edit / request a reviewer / execute.
    /// Default ON (human-in-the-loop before the most expensive phase).
    #[serde(default = "default_true")]
    pub plan_gate_enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            thinker: ModelRef::default(),
            reviewers: vec![ModelRef::default()],
            planning_writer: ModelRef::default(),
            executor_code: ModelRef::default(),
            executor_design: ModelRef::default(),
            executor_reviewer: ModelRef::default(),
            rlm_model: ModelRef::default(),
            rlm_verifier: ModelRef::default(),
            plan_gate_enabled: true,
        }
    }
}

/// Full provider configuration persisted as JSON in the app data directory.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProviderConfig {
    #[serde(default)]
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub agent: AgentConfig,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        let hub_models = vec![
            "thinker".to_string(),
            "thinker_plus".to_string(),
            "reviewer".to_string(),
            "reviewer_cheap".to_string(),
            "planning_writer".to_string(),
            "planning_writer_plus".to_string(),
            "executor_code".to_string(),
            "executor_code_plus".to_string(),
            "executor_design".to_string(),
            "executor_design_cheap".to_string(),
            "executor_reviewer".to_string(),
            "rlm_model".to_string(),
            "rlm_verifier".to_string(),
        ];
        let bind = |model: &str| ModelRef {
            provider_id: "kuda_hub".to_string(),
            model: model.to_string(),
        };
        Self {
            providers: vec![Provider {
                id: "kuda_hub".to_string(),
                name: "Kuda Developer Hub (Subscription Plan)".to_string(),
                base_url: HUB_BASE_URL.to_string(),
                models: hub_models,
            }],
            agent: AgentConfig {
                thinker: bind("thinker"),
                reviewers: vec![bind("reviewer")],
                planning_writer: bind("planning_writer"),
                executor_code: bind("executor_code"),
                executor_design: bind("executor_design"),
                executor_reviewer: bind("executor_reviewer"),
                rlm_model: bind("rlm_model"),
                rlm_verifier: bind("rlm_verifier"),
                plan_gate_enabled: true,
            },
        }
    }
}

pub struct ProviderConfigManager {
    file_path: PathBuf,
}

impl ProviderConfigManager {
    pub fn new(app_data_dir: &Path) -> Result<Self> {
        let file_path = app_data_dir.join("provider_config.json");
        Ok(Self { file_path })
    }

    pub fn load(&self) -> Result<ProviderConfig> {
        if !self.file_path.exists() {
            return Ok(ProviderConfig::default());
        }
        let content = fs::read_to_string(&self.file_path)?;
        let mut cfg: ProviderConfig = serde_json::from_str(&content)?;
        // Migrasi otomatis: kuda_hub provider yang masih menunjuk ke localhost
        // (config lama) diarahkan ke domain publik hub.
        for p in cfg.providers.iter_mut() {
            if p.id == "kuda_hub"
                && (p.base_url.contains("localhost:8090") || p.base_url.contains("127.0.0.1:8090"))
            {
                p.base_url = HUB_BASE_URL.to_string();
            }
        }
        Ok(cfg)
    }

    pub fn save(&self, cfg: &ProviderConfig) -> Result<()> {
        let json = serde_json::to_string_pretty(cfg)?;
        // Atomic write (tmp + fsync + rename) so a crash mid-write can never
        // truncate provider_config.json (credential routing metadata). Mode
        // 0600 mirrors the protection applied to hub_credentials.json.
        let tmp = self.file_path.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp)?;
            f.write_all(json.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.file_path)?;
        Ok(())
    }

    /// Loads a provider by id from disk.
    pub fn load_provider(app_data_dir: &Path, provider_id: &str) -> Result<Provider> {
        let mgr = Self::new(app_data_dir)?;
        let cfg = mgr.load()?;
        cfg.providers
            .into_iter()
            .find(|p| p.id == provider_id)
            .ok_or_else(|| AppError::General(format!("Provider '{}' not found", provider_id)))
    }

    pub fn new_id() -> String {
        format!("prov_{}", Uuid::new_v4().to_string().chars().take(8).collect::<String>())
    }
}