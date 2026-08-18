use tauri::ipc::Channel;
use tauri::{Emitter, State};
use crate::agent::chat_history::{ChatHistoryManager, ChatSessionData, ChatSessionMeta};
use crate::agent::key_store::KeyStore;
use crate::agent::llm_client::{Message, MessageRole};
use crate::agent::orchestrator::{AgentEvent, AgentOrchestrator};
use crate::agent::provider_config::{Provider, ProviderConfigManager};
use crate::agent::roles::resolve_primary_provider;
use crate::agent::swarm::{build_ledger_message, SwarmOrchestrator, TranscriptCollector};
use crate::agent::tool_registry::ToolContext;
use crate::error::{AppError, Result};
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use sha2::{Digest, Sha256};
use rand::RngCore;

#[tauri::command]
pub fn agent_save_key(provider: String, api_key: String) -> Result<()> {
    KeyStore::save_api_key(&provider, &api_key)
}

#[tauri::command]
pub fn agent_has_key(provider: String) -> bool {
    KeyStore::get_api_key(&provider).is_ok()
}

/// Refreshes the rotating Kuda Hub session key now (used right after login/save and
/// by the Settings UI's auto-refresh timer).
#[tauri::command]
pub async fn agent_refresh_hub_session(state: State<'_, AppState>) -> Result<crate::agent::hub_session::HubSessionInfo> {
    let app_data_dir = state.require_app_data_dir()?;
    crate::agent::hub_session::refresh_hub_session(&app_data_dir).await
}

/// Only refreshes the rotating Kuda Hub session key when it is missing or within
/// 5 minutes of expiring. No-op otherwise, so it is cheap to call on a timer.
#[tauri::command]
pub async fn agent_ensure_hub_session(state: State<'_, AppState>) -> Result<()> {
    let app_data_dir = state.require_app_data_dir()?;
    crate::agent::hub_session::ensure_hub_session(&app_data_dir).await
}

/// Persists hub credentials (from OAuth login / save token) into the file-backed
/// store plus a best-effort keychain mirror.
#[tauri::command]
pub async fn agent_save_hub_credentials(
    state: State<'_, AppState>,
    master_token: String,
    session_key: String,
    session_expires_at: String,
    email: String,
    plan_tier: String,
) -> Result<()> {
    let app_data_dir = state.require_app_data_dir()?;
    crate::agent::hub_session::save_hub_credentials(
        &app_data_dir,
        &master_token,
        &session_key,
        &session_expires_at,
        &email,
        &plan_tier,
    )
}

/// Reports whether the Kuda Hub is logged in (file-backed credential store present).
#[tauri::command]
pub fn agent_has_hub_credentials(state: State<'_, AppState>) -> Result<bool> {
    let app_data_dir = state.require_app_data_dir()?;
    Ok(crate::agent::hub_session::HubCredentialStore::has(&app_data_dir))
}

/// Non-network hub account snapshot (email / plan / session expiry) for the
/// Settings "connected" state, automatically ensuring session freshness.
#[tauri::command]
pub async fn agent_hub_account(state: State<'_, AppState>) -> Result<crate::agent::hub_session::HubAccountInfo> {
    let app_data_dir = state.require_app_data_dir()?;
    let _ = crate::agent::hub_session::ensure_hub_session(&app_data_dir).await;
    Ok(crate::agent::hub_session::hub_account(&app_data_dir))
}

/// Mengambil data kuota & saldo koin user dari Hub Server secara terautentikasi
/// menggunakan master token / session key yang tersimpan di backend Rust.
#[tauri::command]
pub async fn agent_hub_user_usage(
    state: State<'_, AppState>,
) -> Result<serde_json::Value> {
    let app_data_dir = state.require_app_data_dir()?;
    let _ = crate::agent::hub_session::ensure_hub_session(&app_data_dir).await;
    let creds = crate::agent::hub_session::HubCredentialStore::load(&app_data_dir)
        .ok_or_else(|| AppError::General("Not logged in to Kuda Hub".to_string()))?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| AppError::General(format!("Failed to build HTTP client: {e}")))?;

    let base_url = crate::agent::provider_config::HUB_BASE_URL.trim_end_matches('/');
    let url = format!("{}/user/usage", base_url);

    let token = if !creds.master_token.is_empty() {
        &creds.master_token
    } else {
        &creds.session_key
    };

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .map_err(|e| AppError::General(format!("Hub network error: {e}")))?;

    if !resp.status().is_success() {
        return Err(AppError::General(format!("Hub returned status {}", resp.status())));
    }

    let val: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AppError::General(format!("Failed to parse usage response: {e}")))?;

    Ok(val)
}

/// Langsung membuat invoice Midtrans Snap dan membuka link pembayaran resmi Midtrans
#[tauri::command]
pub async fn agent_create_snap_checkout(
    state: State<'_, AppState>,
    package_id: String,
    package_name: String,
) -> Result<String> {
    let app_data_dir = state.require_app_data_dir()?;
    let creds = crate::agent::hub_session::HubCredentialStore::load(&app_data_dir);
    let email = creds.map(|c| c.email).filter(|e| !e.is_empty()).unwrap_or_else(|| "dev@kuda.ide".to_string());
    let customer_name = email.split('@').next().unwrap_or("Developer").to_string();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| AppError::General(format!("Failed to build HTTP client: {e}")))?;

    let base_url = crate::agent::provider_config::HUB_BASE_URL.trim_end_matches('/');
    let url = format!("{}/payment/create-snap-invoice", base_url);

    let payload = serde_json::json!({
        "account_id": email,
        "package_id": package_id,
        "package_name": package_name,
        "amount_idr": 0,
        "customer_email": email,
        "customer_name": customer_name
    });

    let resp = client.post(&url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| AppError::General(format!("Gagal menghubungi gateway pembayaran: {e}")))?;

    if !resp.status().is_success() {
        return Err(AppError::General(format!("Gateway pembayaran error: HTTP {}", resp.status())));
    }

    let val: serde_json::Value = resp.json().await
        .map_err(|e| AppError::General(format!("Format invoice tidak valid: {e}")))?;

    let redirect_url = val.get("redirect_url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::General("Link pembayaran tidak ditemukan".to_string()))?;

    // Buka browser langsung ke halaman pembayaran resmi Midtrans Snap
    let _ = crate::commands::project::open_external_url(redirect_url.to_string());

    Ok(redirect_url.to_string())
}

/// Signs out of the hub: removes the file-backed credentials and keychain mirror.
#[tauri::command]
pub fn agent_hub_sign_out(state: State<'_, AppState>) -> Result<()> {
    let app_data_dir = state.require_app_data_dir()?;
    crate::agent::hub_session::clear_hub_credentials(&app_data_dir)
}

/// Polls Hub Server directly from Rust (immune to browser CORS / webview sandbox),
/// and automatically writes credentials to disk immediately upon resolution.
#[tauri::command]
pub async fn agent_poll_hub_login(
    state: State<'_, AppState>,
    verifier: String,
) -> Result<crate::agent::hub_session::HubAccountInfo> {
    let app_data_dir = state.require_app_data_dir()?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| AppError::General(format!("Failed to build HTTP client: {e}")))?;

    let verifier_clean = verifier.trim();
    if verifier_clean.is_empty() {
        return Err(AppError::General("Verifier cannot be empty".to_string()));
    }

    let base_url = crate::agent::provider_config::HUB_BASE_URL.trim_end_matches('/');
    let url = format!("{}/auth/pending?verifier={}", base_url, verifier_clean);

    let resp = client.get(&url).send().await
        .map_err(|e| AppError::General(format!("Hub poll network error: {e}")))?;

    if !resp.status().is_success() {
        return Err(AppError::General(format!("Pending authorization (HTTP {})", resp.status())));
    }

    let auth: crate::agent::hub_session::HubSessionInfo = resp.json().await
        .map_err(|e| AppError::General(format!("Failed to parse Hub auth response: {e}")))?;

    if auth.token_key.is_empty() {
        return Err(AppError::General("Hub returned empty master token".to_string()));
    }

    // Persist immediately to disk in Rust
    crate::agent::hub_session::save_hub_credentials(
        &app_data_dir,
        &auth.token_key,
        &auth.session_key,
        &auth.session_expires_at,
        &auth.email,
        &auth.plan_tier,
    )?;

    Ok(crate::agent::hub_session::HubAccountInfo {
        logged_in: true,
        email: auth.email,
        plan_tier: auth.plan_tier,
        session_expires_at: auth.session_expires_at,
    })
}

/// Inisiasi OAuth GitHub PKCE 100% dari Rust Engine:
/// 1. Rust membuat CSPRNG random verifier (32-byte) & challenge (SHA-256).
/// 2. Rust meminta URL OAuth ke Hub Server dan membuka browser via open_external_url.
/// 3. Rust menjalankan background Tokio task yang terus mem-poll Hub Server (kebal unmount/blur di UI).
/// 4. Saat otorisasi selesai, Rust menulis `hub_credentials.json` ke disk dan memancarkan event `hub-auth-success`.
#[tauri::command]
pub async fn auth_start_github_pkce(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<()> {
    let app_data_dir = state.require_app_data_dir()?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| AppError::General(format!("HTTP client error: {e}")))?;

    // 1. Generate 32-byte cryptographic random verifier & SHA-256 challenge in Rust
    let mut rng = rand::rngs::OsRng;
    let mut verifier_bytes = [0u8; 32];
    rng.fill_bytes(&mut verifier_bytes);
    let verifier = verifier_bytes
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = format!("{:x}", hasher.finalize());

    // 2. Request GitHub OAuth URL from Hub Server
    let base_url = crate::agent::provider_config::HUB_BASE_URL.trim_end_matches('/');
    let redirect_uri = format!("{}/auth/github/callback", base_url);
    let redirect_enc = redirect_uri.replace(":", "%3A").replace("/", "%2F");
    let hub_url = format!(
        "{}/auth/github/url?challenge={}&redirect_uri={}",
        base_url, challenge, redirect_enc
    );

    let resp = client.get(&hub_url).send().await
        .map_err(|e| AppError::General(format!("Gagal menghubungi Hub Server: {e}")))?;

    if !resp.status().is_success() {
        return Err(AppError::General(format!("Hub server error (HTTP {})", resp.status())));
    }

    #[derive(Deserialize)]
    struct HubUrlResp {
        url: String,
    }
    let data: HubUrlResp = resp.json().await
        .map_err(|e| AppError::General(format!("Format respon Hub tidak valid: {e}")))?;

    if data.url.is_empty() {
        return Err(AppError::General("Hub tidak mengembalikan URL OAuth GitHub".to_string()));
    }

    // 3. Open browser from Rust
    let _ = crate::commands::project::open_external_url(data.url);

    // 4. Spawn autonomous Tokio task in Rust
    let poll_client = client.clone();
    let poll_url = format!("{}/auth/pending?verifier={}", base_url, verifier);
    let app_handle = app.clone();

    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let max_duration = std::time::Duration::from_secs(300); // 5 menit

        while started.elapsed() < max_duration {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

            if let Ok(resp) = poll_client.get(&poll_url).send().await {
                if resp.status().is_success() {
                    if let Ok(auth) = resp.json::<crate::agent::hub_session::HubSessionInfo>().await {
                        if !auth.token_key.is_empty() {
                            // Simpan langsung ke disk di Rust
                            let _ = crate::agent::hub_session::save_hub_credentials(
                                &app_data_dir,
                                &auth.token_key,
                                &auth.session_key,
                                &auth.session_expires_at,
                                &auth.email,
                                &auth.plan_tier,
                            );

                            // Emit event ke seluruh UI KudaIDE
                            let account = crate::agent::hub_session::HubAccountInfo {
                                logged_in: true,
                                email: auth.email,
                                plan_tier: auth.plan_tier,
                                session_expires_at: auth.session_expires_at,
                            };
                            let _ = app_handle.emit("hub-auth-success", account);
                            break;
                        }
                    }
                }
            }
        }
    });

    Ok(())
}

/// Spawn loopback HTTP server di 127.0.0.1 untuk OAuth handoff. Kembalikan port
/// yang dipakai (frontend akan menyertakannya ke hub `/auth/github/url`).
#[tauri::command]
pub async fn auth_start_loopback(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<u16> {
    // Shutdown instance lama kalau ada (mis. user klik login ulang).
    {
        let mut slot = state.auth_loopback.lock().await;
        if let Some(mut srv) = slot.take() {
            srv.shutdown().await;
        }
    }
    let server = crate::gateway::auth_loopback::LoopbackServer::spawn(app)
        .await
        .map_err(|e| AppError::General(format!("Failed to start loopback: {e}")))?;
    let port = server.port;
    let mut slot = state.auth_loopback.lock().await;
    *slot = Some(server);
    Ok(port)
}

/// Shutdown loopback server (dipanggil setelah login selesai / gagal / timeout).
#[tauri::command]
pub async fn auth_stop_loopback(state: State<'_, AppState>) -> Result<()> {
    let mut slot = state.auth_loopback.lock().await;
    if let Some(mut srv) = slot.take() {
        srv.shutdown().await;
    }
    Ok(())
}

/// Ambil pickup code yang tersimpan di loopback server saat ini (jika ada).
#[tauri::command]
pub async fn auth_get_pickup(state: State<'_, AppState>) -> Result<Option<String>> {
    let slot = state.auth_loopback.lock().await;
    if let Some(srv) = slot.as_ref() {
        Ok(srv.pickup.lock().await.clone())
    } else {
        Ok(None)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentRunResult {
    pub chat_session_id: String,
    pub edit_session_id: Option<String>,
}

/// Max messages kept verbatim for DIRECT (non-swarm) chat. The swarm has its
/// own ledger windowing; direct chat grew unbounded, which could blow the
/// provider context window and cost. Keep the first user message as an anchor
/// plus the newest `MAX_DIRECT_CHAT_MESSAGES`, trimmed back to a user-role
/// boundary so no assistant/tool turn is left half-formed (strict providers
/// reject dangling tool messages).
const MAX_DIRECT_CHAT_MESSAGES: usize = 80;

fn bound_direct_chat_window(messages: Vec<Message>) -> Vec<Message> {
    if messages.len() <= MAX_DIRECT_CHAT_MESSAGES {
        return messages;
    }
    let keep_from = messages.len() - MAX_DIRECT_CHAT_MESSAGES;
    // Advance to the next user-role message so the retained window starts at a
    // turn boundary.
    let mut start = keep_from;
    while start < messages.len() && messages[start].role != MessageRole::User {
        start += 1;
    }
    if start >= messages.len() {
        // No user boundary found in the tail: keep the tail as-is.
        start = keep_from;
    }
    let mut bounded = vec![messages[0].clone()];
    bounded.extend_from_slice(&messages[start..]);
    bounded
}

#[tauri::command]
pub async fn agent_chat(
    state: State<'_, AppState>,
    user_prompt: String,
    session_id: Option<String>,
    auto_approve: bool,
    run_id: Option<String>,
    on_event: Channel<AgentEvent>,
) -> Result<AgentRunResult> {
    let project_root = state.require_project_root()?;
    let app_data_dir = state.require_app_data_dir()?;
    let tool_registry = state.tool_registry.clone();
    let edit_session_id = run_id
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // Validate ids at the IPC boundary (defense in depth): a hostile frontend
    // must not be able to smuggle path separators into checkpoint/session paths.
    if let Some(id) = &session_id {
        if !id.is_empty() && !crate::agent::chat_history::is_safe_id(id) {
            return Err(AppError::General(
                "Invalid session_id: must be a plain identifier without path separators".to_string(),
            ));
        }
    }
    if !crate::agent::chat_history::is_safe_id(&edit_session_id) {
        return Err(AppError::General(
            "Invalid run_id: must be a plain identifier without path separators".to_string(),
        ));
    }

    // Rotate the Kuda Hub session key first if it is missing or about to expire.
    crate::agent::hub_session::ensure_hub_session(&app_data_dir).await?;

    // Use the same provider resolution as the swarm roles (new provider config
    // first, legacy keychain cluster as fallback). Providers are wrapped in the
    // GatewayHub security pipeline by `roles::wrap_gateway`, so EVERY streamed
    // request passes through JWT validation, device fingerprint binding, intent
    // guard, rate limiting and the audit log.
    let provider = resolve_primary_provider(&app_data_dir)?;

    let history_mgr = ChatHistoryManager::new(&app_data_dir)?;

    let session: ChatSessionData = match &session_id {
        Some(id) if !id.is_empty() => history_mgr.load_session(id)?,
        _ => history_mgr.create_session(None)?,
    };

    let user_message = Message {
        role: MessageRole::User,
        content: user_prompt.clone(),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        created_at: None,
    };
    history_mgr.append_message(&session.meta.session_id, user_message.clone(), None)?;

    let mut context_messages = session.messages.clone();
    context_messages.push(user_message.clone());
    // Window the direct-chat context like the swarm does, so an unbounded
    // session can never blow the provider context window / cost.
    context_messages = bound_direct_chat_window(context_messages);

    let orchestrator = AgentOrchestrator::new(tool_registry);
    let cancel = crate::agent::tool_registry::CancelFlag::new();
    let tool_ctx = ToolContext {
        project_root: project_root.clone(),
        app_data_dir: app_data_dir.clone(),
        external_requests: state.external_requests.clone(),
        plan_decisions: state.plan_decisions.clone(),
        direction_decisions: state.direction_decisions.clone(),
        session_id: Some(edit_session_id.clone()),
        cancel: cancel.clone(),
    };

    state
        .active_runs
        .lock()
        .unwrap()
        .entry(edit_session_id.clone())
        .or_default()
        .push(cancel);
    let run_channel_id = state.external_requests.register_channel(on_event.clone());
    let run_result = orchestrator
        .run_loop(&context_messages, provider, &tool_ctx, auto_approve, &on_event)
        .await;
    state.external_requests.unregister_channel(run_channel_id);
    // NOTE: no `external_requests.cancel_all()` here — the registry is shared
    // app-wide and clearing it would drop ANOTHER concurrent run's pending
    // approvals. Stale entries are removed per-request by the tool itself.
    remove_active_run(&state, &edit_session_id, &tool_ctx.cancel);

    let trace = match run_result {
        Ok(trace) => trace,
        Err(failure) => {
            // Persist everything the model produced before the failure so the
            // session history keeps the conversation even when the run aborts.
            for partial_message in failure.partial_trace {
                history_mgr.append_message(&session.meta.session_id, partial_message, None)?;
            }
            history_mgr.append_message(
                &session.meta.session_id,
                Message {
                    role: MessageRole::Assistant,
                    content: format!("Run failed: {}", failure.error),
                    name: Some("error".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    created_at: None,
                },
                None,
            )?;
            return Err(failure.error);
        }
    };
    for new_message in trace {
        history_mgr.append_message(&session.meta.session_id, new_message, None)?;
    }

    Ok(AgentRunResult {
        chat_session_id: session.meta.session_id,
        edit_session_id: Some(edit_session_id),
    })
}

/// Non-secret config reader used to prefill settings UI (model names / base URL).
/// Secret keys (API keys) are never returned by this command.
#[tauri::command]
pub fn agent_get_config(key: String) -> Option<String> {
    let allowed = key == "openai_base_url"
        || key == "openai_model"
        || key == "gemini_model"
        || key.starts_with("openai_model.")
        || key.starts_with("gemini_model.")
        || key.starts_with("openai_base_url.")
        || key.starts_with("gemini_base_url.");
    if !allowed {
        return None;
    }
    // Keychain-only read: these are non-secret config values (model / base URL);
    // the environment fallback in `get_api_key` would otherwise substitute the
    // catch-all `LLM_API_KEY` for any unknown key.
    KeyStore::get_api_key_from_keychain(&key).ok()
}

/// Deletes a stored non-secret config value (e.g. a model name or base URL) so the
/// user can clear fields / remove reviewer slots. Secret API keys are never deleted.
#[tauri::command]
pub fn agent_delete_config(key: String) -> Result<()> {
    let allowed = key == "openai_base_url"
        || key == "openai_model"
        || key == "gemini_model"
        || key.starts_with("openai_model.")
        || key.starts_with("gemini_model.")
        || key.starts_with("openai_base_url.")
        || key.starts_with("gemini_base_url.");
    if !allowed {
        return Err(AppError::General(format!("Config key '{}' is not deletable", key)));
    }
    // Keychain-only delete (see `agent_get_config`).
    match KeyStore::delete_api_key(&key) {
        Ok(()) => Ok(()),
        // Non-existent entries are treated as already-removed.
        Err(_) => Ok(()),
    }
}

/// Runs the 5-role swarm pipeline (Thinker, Reviewer, Executor Code,
/// Executor Design, Executor Reviewer) over one shared append-only context.
#[tauri::command]
pub async fn agent_swarm_chat(
    state: State<'_, AppState>,
    user_prompt: String,
    session_id: Option<String>,
    auto_approve: bool,
    run_id: Option<String>,
    on_event: Channel<AgentEvent>,
) -> Result<AgentRunResult> {
    let project_root = state.require_project_root()?;
    let app_data_dir = state.require_app_data_dir()?;
    let tool_registry = state.tool_registry.clone();
    let edit_session_id = run_id
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // Validate ids at the IPC boundary (defense in depth).
    if let Some(id) = &session_id {
        if !id.is_empty() && !crate::agent::chat_history::is_safe_id(id) {
            return Err(AppError::General(
                "Invalid session_id: must be a plain identifier without path separators".to_string(),
            ));
        }
    }
    if !crate::agent::chat_history::is_safe_id(&edit_session_id) {
        return Err(AppError::General(
            "Invalid run_id: must be a plain identifier without path separators".to_string(),
        ));
    }

    // Rotate the Kuda Hub session key first if it is missing or about to expire.
    crate::agent::hub_session::ensure_hub_session(&app_data_dir).await?;

    let history_mgr = ChatHistoryManager::new(&app_data_dir)?;
    let session: ChatSessionData = match &session_id {
        Some(id) if !id.is_empty() => history_mgr.load_session(id)?,
        _ => history_mgr.create_session(None)?,
    };

    let user_message = Message {
        role: MessageRole::User,
        content: user_prompt.clone(),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        created_at: None,
    };
    history_mgr.append_message(&session.meta.session_id, user_message.clone(), None)?;

    let orchestrator = SwarmOrchestrator::new(tool_registry);
    let cancel = crate::agent::tool_registry::CancelFlag::new();
    let tool_ctx = ToolContext {
        project_root: project_root.clone(),
        app_data_dir: app_data_dir.clone(),
        external_requests: state.external_requests.clone(),
        plan_decisions: state.plan_decisions.clone(),
        direction_decisions: state.direction_decisions.clone(),
        session_id: Some(edit_session_id.clone()),
        cancel: cancel.clone(),
    };

    // The swarm carries the previous turns of this chat session so follow-up
    // prompts ("change the thing you just did") keep their context. Long
    // sessions are windowed: recent turns verbatim at the front + a prompt that
    // frames the compacted older history as previous chat history.
    let context_messages = crate::agent::chat_history::build_ledger_context(
        &session.messages,
        &user_message,
    );

    state
        .active_runs
        .lock()
        .unwrap()
        .entry(edit_session_id.clone())
        .or_default()
        .push(cancel);
    let run_channel_id = state.external_requests.register_channel(on_event.clone());
    // The transcript collector is owned HERE so a failed run can still persist
    // its partial phases for history replay.
    let transcript_collector: Arc<Mutex<TranscriptCollector>> = Arc::new(Mutex::new(
        TranscriptCollector::new(edit_session_id.clone()),
    ));
    let run_result = orchestrator
        .run_swarm(
            &context_messages,
            None,
            &tool_ctx,
            auto_approve,
            &on_event,
            &transcript_collector,
        )
        .await;
    state.external_requests.unregister_channel(run_channel_id);
    // NOTE: the shared registries are NOT globally cleared here — clearing
    // `external_requests` / `plan_decisions` / `direction_decisions` would drop
    // the pending requests of a CONCURRENT run. Per-request cleanup happens in
    // the tools/gates themselves.
    remove_active_run(&state, &edit_session_id, &tool_ctx.cancel);

    let outcome = match run_result {
        Ok(outcome) => outcome,
        Err(e) => {
            // Persist the failure so the session history explains why the run
            // stopped instead of leaving only the user prompt behind.
            if let Ok(mut col) = transcript_collector.lock() {
                let partial = col.finish();
                if !partial.is_empty() {
                    if let Err(pe) = history_mgr.append_transcript(&session.meta.session_id, &partial) {
                        tracing::warn!("Failed to persist partial transcript: {}", pe);
                    }
                }
            }
            history_mgr.append_message(
                &session.meta.session_id,
                Message {
                    role: MessageRole::Assistant,
                    content: format!("Run failed: {}", e),
                    name: Some("error".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    created_at: None,
                },
                None,
            )?;
            return Err(e);
        }
    };

    // Persist the turn's ledger (ONE assistant message containing brief, plan,
    // plan status, execution review and final answer) as the context for the
    // next turn. Append-only: old blocks are never rewritten.
    let ledger_msg = build_ledger_message(&outcome.ledger);
    history_mgr.append_message(&session.meta.session_id, ledger_msg, None)?;
    // The run finished; drop any resume checkpoint written for it.
    crate::agent::swarm::clear_checkpoint(&app_data_dir, &edit_session_id);
    // Persist the display replay (never sent to the LLM).
    if !outcome.transcript.is_empty() {
        history_mgr.append_transcript(&session.meta.session_id, &outcome.transcript)?;
    }
    // Long sessions: fold turns older than the mandatory recent window into
    // append-only "previous chat history" blocks that are framed inside the next
    // prompt, so the recent-turn prefix stays cache-friendly and verbatim.
    // Hold the session lock across the load→compact→save so a concurrent
    // append cannot interleave and lose the compaction write.
    let _guard = crate::agent::chat_history::SESSION_IO_LOCK.lock().unwrap();
    if let Ok(mut session_for_compact) = history_mgr.load_session(&session.meta.session_id) {
        if let Err(e) = crate::agent::chat_history::compact_epoch(&mut session_for_compact) {
            tracing::warn!("Epoch compaction skipped: {}", e);
        } else if let Err(e) = history_mgr.save_session_unlocked(&session_for_compact) {
            tracing::warn!("Epoch compaction save failed: {}", e);
        }
    }

    Ok(AgentRunResult {
        chat_session_id: session.meta.session_id,
        edit_session_id: Some(edit_session_id),
    })
}

/// Continues a previously-failed swarm run from the phase boundary it last
/// crossed (RLM research done, or direction approved). The checkpoint reuses the
/// validated research brief and the approved direction, so the run resumes at the
/// failed phase instead of restarting from the user prompt.
#[tauri::command]
pub async fn agent_resume_run(
    state: State<'_, AppState>,
    session_id: String,
    run_id: String,
    auto_approve: bool,
    on_event: Channel<AgentEvent>,
) -> Result<AgentRunResult> {
    let project_root = state.require_project_root()?;
    let app_data_dir = state.require_app_data_dir()?;
    let tool_registry = state.tool_registry.clone();

    // Validate ids at the IPC boundary (defense in depth).
    if !crate::agent::chat_history::is_safe_id(&session_id)
        || !crate::agent::chat_history::is_safe_id(&run_id)
    {
        return Err(AppError::General(
            "Invalid session_id/run_id: must be a plain identifier without path separators".to_string(),
        ));
    }

    // Rotate the Kuda Hub session key first if it is missing or about to expire.
    crate::agent::hub_session::ensure_hub_session(&app_data_dir).await?;

    let checkpoint = crate::agent::swarm::load_checkpoint(&app_data_dir, &run_id)
        .ok_or_else(|| {
            AppError::General(
                "No resume point found for this run. Send a new prompt instead.".to_string(),
            )
        })?;

    let history_mgr = ChatHistoryManager::new(&app_data_dir)?;
    let session = history_mgr.load_session(&session_id)?;

    let orchestrator = SwarmOrchestrator::new(tool_registry);
    let cancel = crate::agent::tool_registry::CancelFlag::new();
    let tool_ctx = ToolContext {
        project_root: project_root.clone(),
        app_data_dir: app_data_dir.clone(),
        external_requests: state.external_requests.clone(),
        plan_decisions: state.plan_decisions.clone(),
        direction_decisions: state.direction_decisions.clone(),
        session_id: Some(run_id.clone()),
        cancel: cancel.clone(),
    };

    state
        .active_runs
        .lock()
        .unwrap()
        .entry(run_id.clone())
        .or_default()
        .push(cancel);
    let run_channel_id = state.external_requests.register_channel(on_event.clone());
    // Same run_id so resumed sections group into the original run box in the UI.
    let transcript_collector: Arc<Mutex<TranscriptCollector>> =
        Arc::new(Mutex::new(TranscriptCollector::new(run_id.clone())));
    let resume_shared = checkpoint.shared.clone();
    let run_result = orchestrator
        .run_swarm(
            &resume_shared,
            Some(checkpoint),
            &tool_ctx,
            auto_approve,
            &on_event,
            &transcript_collector,
        )
        .await;
    state.external_requests.unregister_channel(run_channel_id);
    // NOTE: shared registries are NOT globally cleared (see agent_swarm_chat).
    remove_active_run(&state, &run_id, &tool_ctx.cancel);

    let outcome = match run_result {
        Ok(outcome) => outcome,
        Err(e) => {
            // Persist the partial phases so the history shows how far it got.
            if let Ok(mut col) = transcript_collector.lock() {
                let partial = col.finish();
                if !partial.is_empty() {
                    if let Err(pe) =
                        history_mgr.append_transcript(&session.meta.session_id, &partial)
                    {
                        tracing::warn!("Failed to persist partial transcript: {}", pe);
                    }
                }
            }
            history_mgr.append_message(
                &session.meta.session_id,
                Message {
                    role: MessageRole::Assistant,
                    content: format!("Run failed: {}", e),
                    name: Some("error".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    created_at: None,
                },
                None,
            )?;
            return Err(e);
        }
    };

    let ledger_msg = build_ledger_message(&outcome.ledger);
    history_mgr.append_message(&session.meta.session_id, ledger_msg, None)?;
    crate::agent::swarm::clear_checkpoint(&app_data_dir, &run_id);
    if !outcome.transcript.is_empty() {
        history_mgr.append_transcript(&session.meta.session_id, &outcome.transcript)?;
    }
    // Hold the session lock across the load→compact→save so a concurrent
    // append cannot interleave and lose the compaction write.
    let _guard = crate::agent::chat_history::SESSION_IO_LOCK.lock().unwrap();
    if let Ok(mut session_for_compact) = history_mgr.load_session(&session.meta.session_id) {
        if let Err(e) = crate::agent::chat_history::compact_epoch(&mut session_for_compact) {
            tracing::warn!("Epoch compaction skipped: {}", e);
        } else if let Err(e) = history_mgr.save_session_unlocked(&session_for_compact) {
            tracing::warn!("Epoch compaction save failed: {}", e);
        }
    }

    Ok(AgentRunResult {
        chat_session_id: session.meta.session_id,
        edit_session_id: Some(run_id),
    })
}

/// Binds the persistent app-wide event channel. Filesystem commands use it to
/// surface `ExternalAccessRequest` Allow/Deny notifications even when no agent
/// run is active. Safe to call repeatedly (the newest channel wins).
#[tauri::command]
pub fn agent_bind_external_events(
    state: State<'_, AppState>,
    on_event: Channel<AgentEvent>,
) {
    state.external_requests.bind_app(on_event);
}

/// Removes exactly THIS run's cancel flag from its run-id bucket, leaving
/// sibling runs registered under the same id untouched. The bucket itself is
/// dropped once it is empty.
fn remove_active_run(state: &State<'_, AppState>, run_id: &str, flag: &crate::agent::tool_registry::CancelFlag) {
    let mut runs = state.active_runs.lock().unwrap();
    if let Some(bucket) = runs.get_mut(run_id) {
        bucket.retain(|f| !f.ptr_eq(flag));
        if bucket.is_empty() {
            runs.remove(run_id);
        }
    }
}

/// Requests cooperative cancellation of a running agent run identified by the
/// run id (the `edit_session_id` that the frontend generated before invoking
/// `agent_chat` / `agent_swarm_chat`). The run stops at the next turn / chunk /
/// tool boundary.
#[tauri::command]
pub fn agent_cancel_run(state: State<'_, AppState>, run_id: String) {
    // Cancel EVERY flag under this run id: a duplicate id from the frontend
    // must never shield an older run from cancellation.
    if let Some(bucket) = state.active_runs.lock().unwrap().get(&run_id) {
        for flag in bucket {
            flag.cancel();
        }
    }
}

/// Approves a pending external-access request raised by the RLM Model
/// (`request_external_access` tool) or by a filesystem command. The RLM kernel
/// allowlist is then updated by the tool itself so subsequent out-of-project
/// reads succeed.
#[tauri::command]
pub fn agent_approve_external_access(
    state: State<'_, AppState>,
    request_id: String,
) -> Result<()> {
    let ok = state.external_requests.resolve(&request_id, true);
    if ok {
        Ok(())
    } else {
        Err(AppError::General(format!(
            "No pending external access request with id '{}'",
            request_id
        )))
    }
}

/// Denies a pending external-access request raised by the RLM Model.
#[tauri::command]
pub fn agent_deny_external_access(
    state: State<'_, AppState>,
    request_id: String,
) -> Result<()> {
    let _ = state.external_requests.resolve(&request_id, false);
    Ok(())
}

/// Resolves a pending Plan Approval Gate request. `decision` is one of
/// "execute" | "revise" | "review"; `note` carries the user's revision note
/// when decision is "revise".
#[tauri::command]
pub fn agent_resolve_plan_decision(
    state: State<'_, AppState>,
    request_id: String,
    decision: String,
    note: Option<String>,
) -> Result<()> {
    let ok = state
        .plan_decisions
        .resolve(&request_id, decision, note);
    if ok {
        Ok(())
    } else {
        Err(AppError::General(format!(
            "No pending plan decision with id '{}'",
            request_id
        )))
    }
}

/// Resolves a pending Thinker-direction checkpoint request. `decision` is one
/// of "lanjut" (approve the direction, build the full plan) or "ubah" (adjust
/// the direction); `note` carries the user's direction note when "ubah".
#[tauri::command]
pub fn agent_resolve_direction_decision(
    state: State<'_, AppState>,
    request_id: String,
    decision: String,
    note: Option<String>,
) -> Result<()> {
    let ok = state
        .direction_decisions
        .resolve(&request_id, decision, note);
    if ok {
        Ok(())
    } else {
        Err(AppError::General(format!(
            "No pending direction decision with id '{}'",
            request_id
        )))
    }
}

#[tauri::command]
pub fn chat_list_sessions(state: State<'_, AppState>) -> Result<Vec<ChatSessionMeta>> {
    let app_data_dir = state.require_app_data_dir()?;
    let mgr = ChatHistoryManager::new(&app_data_dir)?;
    mgr.list_sessions()
}

#[tauri::command]
pub fn chat_load_session(state: State<'_, AppState>, session_id: String) -> Result<ChatSessionData> {
    let app_data_dir = state.require_app_data_dir()?;
    let mgr = ChatHistoryManager::new(&app_data_dir)?;
    mgr.load_session(&session_id)
}

#[tauri::command]
pub fn chat_delete_session(state: State<'_, AppState>, session_id: String) -> Result<()> {
    let app_data_dir = state.require_app_data_dir()?;
    let mgr = ChatHistoryManager::new(&app_data_dir)?;
    mgr.delete_session(&session_id)
}

/// Provider info returned to the frontend. The API key itself is never exposed.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub models: Vec<String>,
    pub has_key: bool,
}

#[tauri::command]
pub fn provider_list(state: State<'_, AppState>) -> Result<Vec<ProviderInfo>> {
    let app_data_dir = state.require_app_data_dir()?;
    let cfg = ProviderConfigManager::new(&app_data_dir)?.load()?;
    Ok(cfg
        .providers
        .into_iter()
        .map(|p| {
            // Keychain-only check: `get_api_key`'s env fallback would report
            // `has_key = true` for a provider whenever `LLM_API_KEY` happens to
            // be set, even though no key was ever stored for it.
            let has_key = KeyStore::get_api_key_from_keychain(&p.keychain_service()).is_ok();
            ProviderInfo {
                id: p.id,
                name: p.name,
                base_url: p.base_url,
                models: p.models,
                has_key,
            }
        })
        .collect())
}

/// Creates or updates a provider. Persists the provider definition (name, base URL,
/// models) to the config file and stores the API key in the OS Keychain per provider.
#[tauri::command]
pub fn provider_save(
    state: State<'_, AppState>,
    id: Option<String>,
    name: String,
    base_url: String,
    models: Vec<String>,
    api_key: Option<String>,
) -> Result<ProviderInfo> {
    let app_data_dir = state.require_app_data_dir()?;
    let mgr = ProviderConfigManager::new(&app_data_dir)?;
    let mut cfg = mgr.load()?;

    let provider = if let Some(existing_id) = id {
        let mut p = cfg
            .providers
            .iter()
            .find(|p| p.id == existing_id)
            .cloned()
            .ok_or_else(|| AppError::General(format!("Provider '{}' not found", existing_id)))?;
        p.name = name;
        p.base_url = base_url;
        p.models = models;
        p
    } else {
        Provider {
            id: ProviderConfigManager::new_id(),
            name,
            base_url,
            models,
        }
    };

    if let Some(key) = api_key {
        if !key.trim().is_empty() {
            KeyStore::save_api_key(&provider.keychain_service(), key.trim())?;
        }
    }

    if let Some(existing) = cfg.providers.iter_mut().find(|p| p.id == provider.id) {
        *existing = provider.clone();
    } else {
        cfg.providers.push(provider.clone());
    }
    mgr.save(&cfg)?;

    Ok(ProviderInfo {
        has_key: KeyStore::get_api_key(&provider.keychain_service()).is_ok(),
        id: provider.id,
        name: provider.name,
        base_url: provider.base_url,
        models: provider.models,
    })
}

/// Deletes a provider and removes its API key from the Keychain.
#[tauri::command]
pub fn provider_delete(state: State<'_, AppState>, id: String) -> Result<()> {
    let app_data_dir = state.require_app_data_dir()?;
    let mgr = ProviderConfigManager::new(&app_data_dir)?;
    let mut cfg = mgr.load()?;

    if let Some(idx) = cfg.providers.iter().position(|p| p.id == id) {
        let provider = cfg.providers.remove(idx);
        let _ = KeyStore::delete_api_key(&provider.keychain_service());
        mgr.save(&cfg)?;
    }
    Ok(())
}

/// Returns the current agent role-to-model assignment.
#[tauri::command]
pub fn agent_config_get(state: State<'_, AppState>) -> Result<crate::agent::provider_config::AgentConfig> {
    let app_data_dir = state.require_app_data_dir()?;
    let cfg = ProviderConfigManager::new(&app_data_dir)?.load()?;
    Ok(cfg.agent)
}

/// Persists the agent role-to-model assignment (thinker, reviewers, executors).
#[tauri::command]
pub fn agent_config_set(
    state: State<'_, AppState>,
    config: crate::agent::provider_config::AgentConfig,
) -> Result<()> {
    let app_data_dir = state.require_app_data_dir()?;
    let mgr = ProviderConfigManager::new(&app_data_dir)?;
    let mut cfg = mgr.load()?;
    cfg.agent = config;
    mgr.save(&cfg)
}
