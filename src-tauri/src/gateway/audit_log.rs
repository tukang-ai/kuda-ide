use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::gateway::ephemeral_token::TokenClaims;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum AuditEventType {
    TokenIssued,
    TokenExpired,
    TokenRevoked,
    RequestAllowed,
    DeviceMismatch,
    ScopeViolation,
    RateLimitExceeded,
    QuotaExceeded,
    SecretRotated,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AuditEntry {
    pub timestamp: String,
    pub event_type: AuditEventType,
    pub device_hash: String,
    pub details: String,
}

pub struct AuditLog {
    log_file_path: Mutex<Option<PathBuf>>,
}

impl AuditLog {
    pub fn new() -> Self {
        Self {
            log_file_path: Mutex::new(None),
        }
    }

    pub fn init(&self, app_data_dir: &Path) {
        let audit_dir = app_data_dir.join("gateway_audit");
        let _ = std::fs::create_dir_all(&audit_dir);
        let file_path = audit_dir.join("audit.jsonl");
        if let Ok(mut guard) = self.log_file_path.lock() {
            *guard = Some(file_path);
        }
    }

    pub fn log(&self, entry: AuditEntry) {
        tracing::info!(
            "[GATEWAY AUDIT] {:?} - Device: {} - Details: {}",
            entry.event_type,
            entry.device_hash,
            entry.details
        );

        if let Ok(guard) = self.log_file_path.lock() {
            if let Some(ref path) = *guard {
                if let Ok(json) = serde_json::to_string(&entry) {
                    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                        let _ = writeln!(file, "{}", json);
                    }
                }
            }
        }
    }

    pub fn log_success(&self, claims: &TokenClaims) {
        self.log(AuditEntry {
            timestamp: Utc::now().to_rfc3339(),
            event_type: AuditEventType::RequestAllowed,
            device_hash: claims.device_hash.clone(),
            details: format!("Request allowed for scope '{}'", claims.scope),
        });
    }

    pub fn log_violation(&self, event_type: AuditEventType, device_hash: &str, details: &str) {
        self.log(AuditEntry {
            timestamp: Utc::now().to_rfc3339(),
            event_type,
            device_hash: device_hash.to_string(),
            details: details.to_string(),
        });
    }
}
