use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use crate::error::{AppError, Result};
use crate::agent::key_store::KeyStore;
use crate::agent::llm_client::LlmProvider;
use crate::agent::providers::gemini::GeminiProvider;
use crate::agent::providers::openai::OpenAiProvider;
use crate::agent::provider_config::{ModelRef, ProviderConfigManager};

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    Thinker,
    Reviewer,
    PlanningWriter,
    PlanReviewer,
    PlanEditor,
    ExecutorCode,
    ExecutorDesign,
    ExecutorReviewer,
    RlmModel,
    RlmVerifier,
}

impl AgentRole {
    pub fn key(&self) -> &'static str {
        match self {
            AgentRole::Thinker => "thinker",
            AgentRole::Reviewer => "reviewer",
            AgentRole::PlanningWriter => "planning_writer",
            AgentRole::PlanReviewer => "plan_reviewer",
            AgentRole::PlanEditor => "plan_editor",
            AgentRole::ExecutorCode => "executor_code",
            AgentRole::ExecutorDesign => "executor_design",
            AgentRole::ExecutorReviewer => "executor_reviewer",
            AgentRole::RlmModel => "rlm_model",
            AgentRole::RlmVerifier => "rlm_verifier",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            AgentRole::Thinker => "Thinker",
            AgentRole::Reviewer => "Reviewer",
            AgentRole::PlanningWriter => "Planning Writer",
            AgentRole::PlanReviewer => "Plan Reviewer",
            AgentRole::PlanEditor => "Plan Editor",
            AgentRole::ExecutorCode => "Executor Code",
            AgentRole::ExecutorDesign => "Executor Design",
            AgentRole::ExecutorReviewer => "Executor Reviewer",
            AgentRole::RlmModel => "RLM Model",
            AgentRole::RlmVerifier => "RLM Verifier",
        }
    }

    pub fn spec(&self) -> RoleSpec {
        match self {
            AgentRole::Thinker => RoleSpec {
                role: *self,
                max_turns: 6,
                temperature: 0.2,
                allowed_tools: vec![
                    "request_rlm_research".into(),
                ],
            },
            AgentRole::PlanReviewer => RoleSpec {
                role: *self,
                max_turns: 2,
                temperature: 0.1,
                allowed_tools: vec![
                    "submit_plan_review".into(),
                ],
            },
            AgentRole::PlanEditor => RoleSpec {
                role: *self,
                max_turns: 12,
                temperature: 0.1,
                allowed_tools: vec![
                    "batch_file_read".into(),
                    "multi_replace_file".into(),
                    "write_file".into(),
                    "submit_plan".into(),
                ],
            },
            AgentRole::PlanningWriter => RoleSpec {
                role: *self,
                // The writer drafts the FULL detailed plan (the bulk of the
                // plan-writing output) on a cheap model. A few read+write cycles
                // are expected, so the budget is a bit larger than the slim
                // Thinker's. The Thinker only READS the draft and emits a short
                // approve/revise decision — writing is far more expensive than
                // reading, so the expensive model is kept off the plan body.
                max_turns: 24,
                temperature: 0.1,
                allowed_tools: vec![
                    "write_file".into(),
                    "multi_replace_file".into(),
                    "submit_plan".into(),
                ],
            },
            AgentRole::Reviewer => RoleSpec {
                role: *self,
                // The main Reviewer runs on the SMART model (Kimi-K3) and is a
                // READ-ONLY auditor: it audits the finished plan for bugs /
                // logic errors / missing depth and hands DIRECTIONS back to the
                // Thinker via `submit_review_directions`. It never writes files
                // and never rewrites the plan — the Planning Writer does.
                max_turns: 4,
                temperature: 0.2,
                allowed_tools: vec![
                    "batch_file_read".into(),
                    "list_dir".into(),
                    "grep_search".into(),
                    "code_outline".into(),
                    "rlm_python".into(),
                    "submit_review_directions".into(),
                ],
            },
            AgentRole::ExecutorCode => RoleSpec {
                role: *self,
                max_turns: 16,
                temperature: 0.1,
                allowed_tools: vec![
                    "batch_file_read".into(),
                    "multi_replace_file".into(),
                    "write_file".into(),
                    "list_dir".into(),
                    "grep_search".into(),
                    "code_outline".into(),
                    "rlm_python".into(),
                    "run_command".into(),
                ],
            },
            AgentRole::ExecutorDesign => RoleSpec {
                role: *self,
                max_turns: 16,
                temperature: 0.1,
                allowed_tools: vec![
                    "batch_file_read".into(),
                    "multi_replace_file".into(),
                    "write_file".into(),
                    "list_dir".into(),
                    "grep_search".into(),
                    "code_outline".into(),
                    "rlm_python".into(),
                    "run_command".into(),
                ],
            },
            AgentRole::ExecutorReviewer => RoleSpec {
                role: *self,
                max_turns: 10,
                temperature: 0.1,
                allowed_tools: vec![
                    "batch_file_read".into(),
                    "list_dir".into(),
                    "grep_search".into(),
                    "code_outline".into(),
                    "rlm_python".into(),
                    "run_command".into(),
                    "submit_verdict".into(),
                ],
            },
            AgentRole::RlmModel => RoleSpec {
                role: *self,
                max_turns: 24,
                temperature: 0.2,
                allowed_tools: vec![
                    "list_dir".into(),
                    "grep_search".into(),
                    "code_outline".into(),
                    "batch_file_read".into(),
                    "rlm_python".into(),
                    "request_external_access".into(),
                    "run_command".into(),
                    // The model WRITES the complete brief to .kuda/brief.md itself
                    // (code pasted verbatim + explanation), then submit_brief only
                    // points at the file — no response-text handoff, no placeholders.
                    "write_file".into(),
                    "submit_brief".into(),
                ],
            },
            AgentRole::RlmVerifier => RoleSpec {
                role: *self,
                max_turns: 6,
                temperature: 0.1,
                allowed_tools: vec![
                    "batch_file_read".into(),
                    "list_dir".into(),
                    "grep_search".into(),
                    "code_outline".into(),
                    "rlm_python".into(),
                    "run_command".into(),
                    "submit_audit".into(),
                ],
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct RoleSpec {
    pub role: AgentRole,
    pub max_turns: usize,
    pub temperature: f32,
    pub allowed_tools: Vec<String>,
}

pub const SMART_ROLES: [AgentRole; 3] = [
    AgentRole::Thinker,
    AgentRole::Reviewer,
    AgentRole::ExecutorReviewer,
];

impl AgentRole {
    /// Smart tier roles use the expensive model; executor roles use cheaper models.
    pub fn is_smart_tier(&self) -> bool {
        matches!(
            self,
            AgentRole::Thinker | AgentRole::Reviewer | AgentRole::ExecutorReviewer
        )
    }
}

/// Maximum number of configurable reviewer slots.
pub const MAX_REVIEWER_SLOTS: usize = 5;

/// Returns the model references for a role from the persisted agent config.
fn role_model_refs(role: AgentRole, cfg: &crate::agent::provider_config::AgentConfig) -> Vec<ModelRef> {
    match role {
        AgentRole::Thinker => vec![cfg.thinker.clone()],
        AgentRole::PlanningWriter => vec![cfg.planning_writer.clone()],
        AgentRole::PlanReviewer => vec![cfg.plan_reviewer.clone()],
        AgentRole::PlanEditor => vec![cfg.plan_editor.clone()],
        AgentRole::Reviewer => {
            let mut refs = cfg.reviewers.clone();
            if refs.is_empty() {
                refs.push(ModelRef::default());
            }
            refs.iter().take(MAX_REVIEWER_SLOTS).cloned().collect()
        }
        AgentRole::ExecutorCode => vec![cfg.executor_code.clone()],
        AgentRole::ExecutorDesign => vec![cfg.executor_design.clone()],
        AgentRole::ExecutorReviewer => vec![cfg.executor_reviewer.clone()],
        AgentRole::RlmModel => vec![cfg.rlm_model.clone()],
        AgentRole::RlmVerifier => vec![cfg.rlm_verifier.clone()],
    }
}

/// Wraps a freshly-resolved provider in the GatewayHub security pipeline when a
/// gateway is available (installed by `AppState::new`). Falls back to the raw
/// provider when no gateway / token is available so a fingerprint failure never
/// bricks chat.
fn wrap_gateway(p: Arc<dyn LlmProvider>) -> Arc<dyn LlmProvider> {
    match crate::state::global_gateway() {
        Some(gateway) => Arc::new(crate::gateway::gateway_hub::GatewayProvider {
            inner: p,
            gateway,
        }),
        None => p,
    }
}

/// Builds an OpenAI-compatible provider for a bound model reference.
fn build_provider(app_data_dir: &Path, model_ref: &ModelRef) -> Result<Arc<dyn LlmProvider>> {
    let provider = ProviderConfigManager::load_provider(app_data_dir, &model_ref.provider_id)?;
    // Every provider's base URL is validated (https, or http on loopback only)
    // so a rewritten `provider_config.json` can never attach the provider's API
    // key to an arbitrary host in plaintext — previously only the kuda_hub
    // provider was checked.
    let base_url = crate::agent::hub_session::validate_base_url(
        &provider.base_url,
        &provider.name,
    )?;
    let api_key = if model_ref.provider_id == "kuda_hub" {
        crate::agent::hub_session::HubCredentialStore::load(app_data_dir)
            .map(|c| c.session_key)
            .filter(|k| !k.is_empty())
            .or_else(|| KeyStore::get_api_key(&provider.keychain_service()).ok())
            .ok_or_else(|| AppError::General("Kuda Hub session key not found. Log in via Settings -> Developer Subscription.".to_string()))?
    } else {
        KeyStore::get_api_key(&provider.keychain_service())?
    };
    let model = if model_ref.model.trim().is_empty() {
        provider.models.first().cloned()
    } else {
        Some(model_ref.model.clone())
    };
    let mut openai = OpenAiProvider::new(
        api_key,
        Some(base_url),
        model,
    );
    if model_ref.provider_id == "kuda_hub" {
        let app_dir_buf = app_data_dir.to_path_buf();
        openai = openai.with_key_resolver(Arc::new(move || {
            crate::agent::hub_session::HubCredentialStore::load(&app_dir_buf)
                .map(|c| c.session_key)
                .filter(|k| !k.is_empty())
        }));
    }
    Ok(wrap_gateway(Arc::new(openai)))
}

/// Resolves an LLM provider for a specific agent role.
///
/// Looks up the role's bound provider + model in the persisted provider config, then
/// loads that provider's own API key from the OS Keychain.
pub async fn resolve_role_provider(role: AgentRole, app_data_dir: &Path) -> Result<Arc<dyn LlmProvider>> {
    resolve_role_providers(role, app_data_dir)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::General(format!("No provider configured for role '{}'", role.key())))
}

/// Resolves one or more LLM providers for a role.
///
/// Most roles yield exactly one provider. The Reviewer role may yield several provider
/// instances (one per configured reviewer slot) so multiple reviewers run in sequence.
pub async fn resolve_role_providers(role: AgentRole, app_data_dir: &Path) -> Result<Vec<Arc<dyn LlmProvider>>> {
    // Refresh the rotating hub session key BEFORE any provider is built. The
    // provider bakes the key into itself, and the key rotates/expires server-side
    // every 30 minutes — so a swarm paused at a gate can otherwise continue with a
    // stale key and get 401 mid-run. Cheap no-op when the key is still fresh.
    let _ = crate::agent::hub_session::ensure_hub_session(app_data_dir).await;
    let mgr = ProviderConfigManager::new(app_data_dir)?;
    let cfg = mgr.load()?;

    // If no providers are configured yet, fall back to the legacy single-cluster
    // keychain config so existing installations keep working.
    if cfg.providers.is_empty() {
        return resolve_legacy_role_provider(role);
    }

    let mut providers: Vec<Arc<dyn LlmProvider>> = Vec::new();
    for model_ref in role_model_refs(role, &cfg.agent) {
        if model_ref.provider_id.trim().is_empty() {
            continue;
        }
        match build_provider(app_data_dir, &model_ref) {
            Ok(p) => providers.push(p),
            Err(e) => {
                // A broken reviewer slot should not fail the whole run; skip it.
                tracing::warn!("Skipping unconfigured role binding: {}", e);
            }
        }
    }

    if providers.is_empty() {
        return Err(AppError::General(format!(
            "No model configured for role '{}'. Add a provider and assign models in Settings.",
            role.display_name()
        )));
    }
    Ok(providers)
}

/// Resolves the primary provider used by DIRECT (non-swarm) chat. Prefers the
/// new provider config (Thinker binding) when providers are configured, and
/// falls back to the legacy keychain cluster (openai/gemini) otherwise — so
/// direct chat and swarm use the same configuration system.
pub fn resolve_primary_provider(app_data_dir: &Path) -> Result<Arc<dyn LlmProvider>> {
    let mgr = ProviderConfigManager::new(app_data_dir)?;
    let cfg = mgr.load()?;
    if !cfg.providers.is_empty() {
        for model_ref in role_model_refs(AgentRole::Thinker, &cfg.agent) {
            if model_ref.provider_id.trim().is_empty() {
                continue;
            }
            match build_provider(app_data_dir, &model_ref) {
                Ok(p) => return Ok(p),
                Err(e) => tracing::warn!("Skipping direct-chat provider binding: {}", e),
            }
        }
    }
    resolve_legacy_role_provider(AgentRole::Thinker)?
        .into_iter()
        .next()
        .ok_or_else(|| {
            AppError::General(
                "API key not configured. Save a Gemini or OpenAI/DeepSeek key via Settings -> API Keys."
                    .to_string(),
            )
        })
}

/// Legacy fallback: resolve from the single keychain cluster (openai/gemini).
fn resolve_legacy_role_provider(role: AgentRole) -> Result<Vec<Arc<dyn LlmProvider>>> {
    if let Ok(openai_key) = KeyStore::get_api_key("openai") {
        let base_url = KeyStore::get_api_key_from_keychain("openai_base_url")
            .ok()
            // Same scheme validation as the provider-config path: a legacy
            // base_url (https, or http only on loopback) can never attach the
            // API key to an arbitrary host in plaintext.
            .map(|b| crate::agent::hub_session::validate_base_url(&b, "legacy OpenAI provider"))
            .transpose()?;
        let smart_model = KeyStore::get_api_key_from_keychain("openai_model").ok();
        let model = if role.is_smart_tier() {
            smart_model
        } else {
            KeyStore::get_api_key_from_keychain(&format!("openai_model.{}", role.key()))
                .ok()
                .or(smart_model)
        };
        return Ok(vec![wrap_gateway(Arc::new(OpenAiProvider::new(openai_key, base_url, model)))]);
    }

    if let Ok(gemini_key) = KeyStore::get_api_key("gemini") {
        let model = KeyStore::get_api_key_from_keychain(&format!("gemini_model.{}", role.key()))
            .ok()
            .or_else(|| KeyStore::get_api_key_from_keychain("gemini_model").ok());
        return Ok(vec![wrap_gateway(Arc::new(GeminiProvider::new(gemini_key, model)))]);
    }

    Err(AppError::General(
        "API key not configured. Save a Gemini or OpenAI/DeepSeek key via Settings -> API Keys.".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_keys_unique() {
        let roles = [
            AgentRole::Thinker,
            AgentRole::Reviewer,
            AgentRole::PlanningWriter,
            AgentRole::PlanReviewer,
            AgentRole::ExecutorCode,
            AgentRole::ExecutorDesign,
            AgentRole::ExecutorReviewer,
            AgentRole::RlmModel,
            AgentRole::RlmVerifier,
        ];
        let mut keys: Vec<&str> = roles.iter().map(|r| r.key()).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), 9);
    }

    #[test]
    fn test_cheap_tier_roles_are_not_smart() {
        assert!(!AgentRole::ExecutorCode.is_smart_tier());
        assert!(!AgentRole::ExecutorDesign.is_smart_tier());
        assert!(!AgentRole::RlmModel.is_smart_tier());
        assert!(!AgentRole::RlmVerifier.is_smart_tier());
        assert!(!AgentRole::PlanningWriter.is_smart_tier());
        assert!(!AgentRole::PlanReviewer.is_smart_tier());
        assert!(AgentRole::Thinker.is_smart_tier());
        assert!(AgentRole::Reviewer.is_smart_tier());
        assert!(AgentRole::ExecutorReviewer.is_smart_tier());
    }

    #[test]
    fn test_rlm_verifier_is_read_only_plus_shell() {
        let spec = AgentRole::RlmVerifier.spec();
        assert!(spec.allowed_tools.contains(&"submit_audit".to_string()));
        assert!(spec.allowed_tools.contains(&"run_command".to_string()));
        for forbidden in ["multi_replace_file", "write_file", "submit_plan", "submit_brief", "request_external_access"] {
            assert!(
                !spec.allowed_tools.contains(&forbidden.to_string()),
                "RlmVerifier must not have mutating/handoff tool {}",
                forbidden
            );
        }
    }

    #[test]
    fn test_rlm_model_has_brief_and_access_tools() {
        let spec = AgentRole::RlmModel.spec();
        assert!(spec.allowed_tools.contains(&"submit_brief".to_string()));
        assert!(spec.allowed_tools.contains(&"request_external_access".to_string()));
        assert!(spec.allowed_tools.contains(&"rlm_python".to_string()));
        assert!(spec.allowed_tools.contains(&"run_command".to_string()));
        // The model WRITES the brief artifact itself (code pasted verbatim +
        // explanation) but stays read-only on the codebase.
        assert!(spec.allowed_tools.contains(&"write_file".to_string()));
        for forbidden in ["multi_replace_file", "submit_plan", "submit_audit", "submit_verdict"] {
            assert!(
                !spec.allowed_tools.contains(&forbidden.to_string()),
                "RlmModel must not have code-mutation/plan/audit/verdict tool {}",
                forbidden
            );
        }
    }

    #[test]
    fn test_thinker_is_slim_after_brief() {
        let spec = AgentRole::Thinker.spec();
        assert!(spec.allowed_tools.contains(&"request_rlm_research".to_string()));
        for forbidden in ["list_dir", "grep_search", "rlm_python", "run_command", "multi_replace_file", "batch_file_read", "write_file", "submit_plan_review", "submit_plan"] {
            assert!(
                !spec.allowed_tools.contains(&forbidden.to_string()),
                "slim Thinker must not have I/O exploration or mutation tool {}",
                forbidden
            );
        }
    }

    #[test]
    fn test_executors_cannot_submit_plan() {
        for role in [AgentRole::ExecutorCode, AgentRole::ExecutorDesign] {
            let spec = role.spec();
            assert!(!spec.allowed_tools.contains(&"submit_plan".to_string()));
            assert!(spec.allowed_tools.contains(&"multi_replace_file".to_string()));
        }
    }

    #[test]
    fn test_planning_writer_writes_the_plan_file() {
        let spec = AgentRole::PlanningWriter.spec();
        // The writer owns the plan body: it writes .kuda/plan.md and submits it.
        assert!(spec.allowed_tools.contains(&"submit_plan".to_string()));
        assert!(spec.allowed_tools.contains(&"write_file".to_string()));
        assert!(spec.allowed_tools.contains(&"multi_replace_file".to_string()));
        // It must NOT read files, mutate code files or execute commands.
        for forbidden in [
            "batch_file_read",
            "list_dir",
            "grep_search",
            "code_outline",
            "rlm_python",
            "run_command",
            "submit_brief",
            "submit_audit",
            "submit_verdict",
        ] {
            assert!(
                !spec.allowed_tools.contains(&forbidden.to_string()),
                "PlanningWriter must not have exploration/mutation tool {}",
                forbidden
            );
        }
        // It is a cheap-tier role that does the bulk writing.
        assert!(!AgentRole::PlanningWriter.is_smart_tier());
    }

    #[test]
    fn test_plan_reviewer_spec() {
        let spec = AgentRole::PlanReviewer.spec();
        assert!(spec.allowed_tools.contains(&"submit_plan_review".to_string()));
        for forbidden in ["write_file", "multi_replace_file", "submit_plan", "batch_file_read", "run_command"] {
            assert!(
                !spec.allowed_tools.contains(&forbidden.to_string()),
                "PlanReviewer must not have tool {}",
                forbidden
            );
        }
        assert!(!AgentRole::PlanReviewer.is_smart_tier());
    }

    #[test]
    fn test_plan_editor_spec() {
        let spec = AgentRole::PlanEditor.spec();
        assert!(spec.allowed_tools.contains(&"submit_plan".to_string()));
        assert!(spec.allowed_tools.contains(&"batch_file_read".to_string()));
        assert!(spec.allowed_tools.contains(&"multi_replace_file".to_string()));
        assert!(spec.allowed_tools.contains(&"write_file".to_string()));
        for forbidden in ["list_dir", "grep_search", "code_outline", "rlm_python", "run_command"] {
            assert!(
                !spec.allowed_tools.contains(&forbidden.to_string()),
                "PlanEditor must not have tool {}",
                forbidden
            );
        }
        assert!(!AgentRole::PlanEditor.is_smart_tier());
    }
}
