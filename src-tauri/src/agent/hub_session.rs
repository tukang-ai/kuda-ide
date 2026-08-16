use crate::agent::key_store::KeyStore;
use crate::agent::provider_config::ProviderConfigManager;
use crate::error::{AppError, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// Keychain entry holding the permanent master developer token (`kuda_tok_...`).
/// Used only to obtain fresh rotating session keys; never sent on chat requests.
const MASTER_KEY: &str = "kuda_hub_master";
/// Keychain entry holding the RFC3339 expiry of the current session key.
const SESSION_EXPIRY_KEY: &str = "kuda_hub_session_expiry";
const SESSION_KEY_PREFIX: &str = "kuda_sk_";
/// Refresh at most this long before the session key expires. 10 minutes so a
/// mid-run refresh (before each phase resolves its provider) still leaves a wide
/// safety window for long swarms paused at a gate.
const REFRESH_AHEAD: chrono::Duration = chrono::Duration::minutes(10);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubSessionInfo {
    pub token_key: String,
    pub session_key: String,
    pub session_expires_at: String,
    pub email: String,
    pub plan_tier: String,
}

/// Hub credentials persisted as `<app_data_dir>/hub_credentials.json` (mode 0600).
///
/// The OS Keychain is unreliable for freshly built dev binaries (macOS per-app
/// keychains + code-signature ACLs change every rebuild), so this file-backed store
/// is the source of truth for the rotating hub session. Keychain writes are kept as
/// a best-effort mirror for compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubCredentials {
    pub master_token: String,
    pub session_key: String,
    pub session_expires_at: String,
    pub email: String,
    pub plan_tier: String,
}

pub struct HubCredentialStore;

impl HubCredentialStore {
    pub fn path(app_data_dir: &Path) -> PathBuf {
        app_data_dir.join("hub_credentials.json")
    }

    pub fn load(app_data_dir: &Path) -> Option<HubCredentials> {
        let content = std::fs::read_to_string(Self::path(app_data_dir)).ok()?;
        serde_json::from_str(&content).ok()
    }

    pub fn save(app_data_dir: &Path, creds: &HubCredentials) -> std::io::Result<()> {
        let path = Self::path(app_data_dir);
        let json = serde_json::to_string_pretty(creds).unwrap_or_default();
        // Atomic write (temp file + rename) so a crash mid-write can never
        // truncate the master token store.
        let tmp = app_data_dir.join(format!(".hub_credentials.json.tmp.{}", std::process::id()));
        {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            opts.mode(0o600);
            let mut f = opts.open(&tmp)?;
            f.write_all(json.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &path)
    }

    pub fn has(app_data_dir: &Path) -> bool {
        Self::load(app_data_dir).is_some()
    }

    /// Mirrors the credentials into the OS Keychain (best effort only).
    fn mirror_to_keychain(creds: &HubCredentials) {
        let _ = KeyStore::save_api_key(MASTER_KEY, &creds.master_token);
        let _ = KeyStore::save_api_key(SESSION_EXPIRY_KEY, &creds.session_expires_at);
        let _ = KeyStore::save_api_key("provider_key.kuda_hub", &creds.session_key);
    }
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    #[serde(default)]
    token_key: String,
    #[serde(default)]
    session_key: String,
    #[serde(default)]
    session_expires_at: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    plan_tier: String,
}

/// Validates the Hub base URL before any credential is attached to a request.
///
/// Policy: `https://` is always allowed; plain `http://` is allowed ONLY for
/// loopback hosts (`localhost`, `127.0.0.0/8`, `::1`) so the local dev hub
/// keeps working. Anything else (a rewritten `provider_config.json` pointing
/// at an attacker host, exotic schemes, URLs with embedded credentials) is
/// rejected — otherwise the permanent master token would be sent there as a
/// Bearer credential.
/// Validates an LLM provider base URL: `https` anywhere, or `http` only on
/// loopback hosts (`localhost`, `127.0.0.0/8`, `::1`) so local dev servers
/// (the Kuda Hub on :8090, LM Studio, Ollama, vLLM on 127.0.0.1) keep working.
/// Anything else (a rewritten `provider_config.json` pointing at an attacker
/// host, exotic schemes, URLs with embedded credentials) is rejected — otherwise
/// the provider's API key would be sent there as a Bearer credential in
/// plaintext.
pub fn validate_base_url(base_url: &str, what: &str) -> Result<String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(trimmed).map_err(|e| {
        AppError::General(format!("Invalid {} base URL '{}': {}", what, base_url, e))
    })?;
    let has_userinfo = !url.username().is_empty() || url.password().is_some();
    if has_userinfo {
        return Err(AppError::General(format!(
            "{} base URL '{}' must not contain credentials",
            what, base_url
        )));
    }
    let host = url.host_str().unwrap_or_default();
    // Bracketed IPv6 literals (`[::1]`) come back with brackets.
    let host_trimmed = host.trim_start_matches('[').trim_end_matches(']');
    let is_loopback = host_trimmed == "localhost"
        || host_trimmed
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    match url.scheme() {
        "https" => Ok(trimmed.to_string()),
        "http" if is_loopback => Ok(trimmed.to_string()),
        scheme => Err(AppError::General(format!(
            "{} base URL must use https (http is only allowed on loopback), got '{}'. \
             Fix the provider base URL in Settings.",
            what, scheme
        ))),
    }
}

/// Kuda Hub flavor of [`validate_base_url`] (kept for compatibility with the
/// existing call sites and error copy).
pub fn validate_hub_base_url(base_url: &str) -> Result<String> {
    validate_base_url(base_url, "Kuda Hub")
}

pub fn has_master_token(app_data_dir: &Path) -> bool {
    if let Some(creds) = HubCredentialStore::load(app_data_dir) {
        return !creds.master_token.is_empty();
    }
    KeyStore::get_api_key(MASTER_KEY).is_ok()
}

fn session_expiry(app_data_dir: &Path) -> Option<DateTime<Utc>> {
    if let Some(creds) = HubCredentialStore::load(app_data_dir) {
        if let Ok(dt) = DateTime::parse_from_rfc3339(&creds.session_expires_at) {
            return Some(dt.with_timezone(&Utc));
        }
    }
    KeyStore::get_api_key(SESSION_EXPIRY_KEY)
        .ok()
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|d| d.with_timezone(&Utc))
}

/// True when the stored session key is missing, is not a session key (e.g. a legacy
/// master token saved as the provider key), or is within 5 minutes of expiring.
fn session_needs_refresh(app_data_dir: &Path) -> bool {
    let session = HubCredentialStore::load(app_data_dir)
        .map(|c| c.session_key)
        .or_else(|| KeyStore::get_api_key("provider_key.kuda_hub").ok());
    match session {
        Some(s) if s.starts_with(SESSION_KEY_PREFIX) => match session_expiry(app_data_dir) {
            Some(exp) => Utc::now() + REFRESH_AHEAD >= exp,
            None => true,
        },
        _ => true,
    }
}

/// Saves hub credentials (obtained from OAuth login or a refresh) into the file store
/// and mirrors them into the OS Keychain.
pub fn save_hub_credentials(
    app_data_dir: &Path,
    master_token: &str,
    session_key: &str,
    session_expires_at: &str,
    email: &str,
    plan_tier: &str,
) -> Result<()> {
    let creds = HubCredentials {
        master_token: master_token.to_string(),
        session_key: session_key.to_string(),
        session_expires_at: session_expires_at.to_string(),
        email: email.to_string(),
        plan_tier: plan_tier.to_string(),
    };
    HubCredentialStore::save(app_data_dir, &creds)
        .map_err(|e| AppError::General(format!("Failed to store hub credentials: {}", e)))?;
    HubCredentialStore::mirror_to_keychain(&creds);
    Ok(())
}

/// Always calls the Hub Server's `/auth/refresh` endpoint with the master token and
/// persists the fresh rotating session key + expiry into the file store (and keychain
/// mirror). The session key is what the kuda_hub provider uses as its Bearer credential.
pub async fn refresh_hub_session(app_data_dir: &Path) -> Result<HubSessionInfo> {
    let base_url = match ProviderConfigManager::load_provider(app_data_dir, "kuda_hub") {
        Ok(p) => validate_hub_base_url(&p.base_url)?,
        Err(_) => {
            return Err(AppError::General(
                "Kuda Hub provider is not configured".to_string(),
            ))
        }
    };
    let master = HubCredentialStore::load(app_data_dir)
        .map(|c| c.master_token)
        .or_else(|| KeyStore::get_api_key(MASTER_KEY).ok())
        .filter(|m| !m.is_empty())
        .ok_or_else(|| {
            AppError::General(
                "No Kuda Hub master token saved. Log in via Settings -> Developer Subscription."
                    .to_string(),
            )
        })?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| AppError::General(format!("Failed to build HTTP client: {}", e)))?;

    let resp = client
        .post(format!("{}/auth/refresh", base_url))
        .header("Authorization", format!("Bearer {}", master))
        .send()
        .await
        .map_err(|e| AppError::General(format!("Kuda Hub refresh failed: {}", e)))?;

    let status = resp.status();
    if !status.is_success() {
        let err_text = resp.text().await.unwrap_or_default();
        return Err(AppError::General(format!(
            "Kuda Hub refresh failed ({}): {}",
            status,
            err_text.chars().take(300).collect::<String>()
        )));
    }

    let parsed: RefreshResponse = resp
        .json()
        .await
        .map_err(|e| AppError::General(format!("Kuda Hub refresh parse error: {}", e)))?;

    if parsed.session_key.is_empty() {
        return Err(AppError::General(
            "Kuda Hub returned an empty session key".to_string(),
        ));
    }

    // Persist the fresh rotating session key but KEEP the existing master token.
    // `/auth/refresh` returns `token_key` as the session's id/key, NOT a new
    // master token — overwriting the master with it used to break the NEXT
    // rotation (the store then held a non-master token and refresh started
    // failing with 401). Fall back to `token_key` only when nothing is stored.
    let master_to_keep = HubCredentialStore::load(app_data_dir)
        .map(|c| c.master_token)
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| parsed.token_key.clone());
    save_hub_credentials(
        app_data_dir,
        &master_to_keep,
        &parsed.session_key,
        &parsed.session_expires_at,
        &parsed.email,
        &parsed.plan_tier,
    )?;

    Ok(HubSessionInfo {
        token_key: parsed.token_key,
        session_key: parsed.session_key,
        session_expires_at: parsed.session_expires_at,
        email: parsed.email,
        plan_tier: parsed.plan_tier,
    })
}

/// Called before a chat run. Only contacts the Hub Server when the stored session key
/// is missing or within 5 minutes of expiring (the server rotates keys 30-minutely and
/// waits for idle). If the hub is unreachable, the existing key is kept so the real
/// error surfaces at request time instead of blocking startup.
pub async fn ensure_hub_session(app_data_dir: &Path) -> Result<()> {
    if !has_master_token(app_data_dir) {
        return Ok(()); // no hub login configured; nothing to rotate
    }
    if !session_needs_refresh(app_data_dir) {
        return Ok(());
    }
    match refresh_hub_session(app_data_dir).await {
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::warn!("Kuda Hub session refresh skipped: {}", e);
            Ok(())
        }
    }
}

/// Non-network snapshot of the logged-in hub account (read from the file-backed
/// credential store) so the UI can show a "connected" state without hitting the
/// server. `logged_in` is false when no credentials are stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubAccountInfo {
    pub logged_in: bool,
    pub email: String,
    pub plan_tier: String,
    pub session_expires_at: String,
}

pub fn hub_account(app_data_dir: &Path) -> HubAccountInfo {
    match HubCredentialStore::load(app_data_dir) {
        Some(c) => HubAccountInfo {
            logged_in: true,
            email: c.email,
            plan_tier: c.plan_tier,
            session_expires_at: c.session_expires_at,
        },
        None => HubAccountInfo {
            logged_in: false,
            email: String::new(),
            plan_tier: String::new(),
            session_expires_at: String::new(),
        },
    }
}

/// Removes the stored hub credentials (file store + keychain mirror). Used by the
/// "Sign out" action in Settings; chat then falls back to non-hub providers.
pub fn clear_hub_credentials(app_data_dir: &Path) -> Result<()> {
    let path = HubCredentialStore::path(app_data_dir);
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| AppError::General(format!("Failed to remove hub credentials: {}", e)))?;
    }
    let _ = KeyStore::delete_api_key(MASTER_KEY);
    let _ = KeyStore::delete_api_key(SESSION_EXPIRY_KEY);
    let _ = KeyStore::delete_api_key("provider_key.kuda_hub");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_base_url_policy_blocks_exfil_targets() {
        // https is always fine.
        assert!(validate_hub_base_url("https://hub.kuda.dev/api/v1").is_ok());
        // Plain http is fine ONLY on loopback (local dev hub).
        assert!(validate_hub_base_url("http://localhost:8090/api/v1").is_ok());
        assert!(validate_hub_base_url("http://127.0.0.1:8090/api/v1").is_ok());
        // http to any non-loopback host would leak the master token — rejected.
        assert!(validate_hub_base_url("http://attacker.example/api/v1").is_err());
        // Exotic schemes / credentials / garbage are rejected.
        assert!(validate_hub_base_url("ftp://hub.kuda.dev").is_err());
        assert!(validate_hub_base_url("https://user:pass@hub.kuda.dev").is_err());
        assert!(validate_hub_base_url("not a url").is_err());
    }

    #[test]
    fn file_credential_store_roundtrip() {
        let dir = std::env::temp_dir().join(format!("kuda_hub_store_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let creds = HubCredentials {
            master_token: "kuda_tok_test".to_string(),
            session_key: "kuda_sk_test".to_string(),
            session_expires_at: "2026-08-12T00:00:00+00:00".to_string(),
            email: "dev@kuda.ide".to_string(),
            plan_tier: "developer".to_string(),
        };
        HubCredentialStore::save(&dir, &creds).expect("save should succeed");
        assert!(HubCredentialStore::has(&dir));

        let loaded = HubCredentialStore::load(&dir).expect("load should succeed");
        assert_eq!(loaded.master_token, "kuda_tok_test");
        assert_eq!(loaded.session_key, "kuda_sk_test");
        assert_eq!(loaded.plan_tier, "developer");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
