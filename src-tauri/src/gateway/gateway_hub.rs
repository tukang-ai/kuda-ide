use std::pin::Pin;
use std::sync::Arc;
use async_trait::async_trait;
use futures::{Stream, StreamExt};

use crate::agent::llm_client::{ChunkKind, CompletionRequest, LlmProvider, StreamChunk};
use crate::error::{AppError, Result};
use crate::gateway::audit_log::{AuditEventType, AuditLog};
use crate::gateway::device_fingerprint::DeviceFingerprint;
use crate::gateway::ephemeral_token::EphemeralTokenManager;
use crate::gateway::intent_guard::IntentGuard;
use crate::gateway::rate_limiter::{RateLimitConfig, RateLimiter};
use crate::gateway::secure_vault::SecureVault;

pub struct GatewayHub {
    token_manager: Arc<EphemeralTokenManager>,
    fingerprint_store: Arc<DeviceFingerprint>,
    intent_guard: Arc<IntentGuard>,
    rate_limiter: Arc<RateLimiter>,
    secure_vault: Arc<SecureVault>,
    audit_log: Arc<AuditLog>,
}

impl GatewayHub {
    pub fn new() -> Self {
        // NEVER hardcode a JWT secret. Derive a random 256-bit secret for this
        // process (the gateway is local: tokens are minted + validated in-process,
        // so they do not need to survive a restart).
        let secret = random_secret();
        Self {
            token_manager: Arc::new(EphemeralTokenManager::new(&secret)),
            fingerprint_store: Arc::new(DeviceFingerprint::new()),
            intent_guard: Arc::new(IntentGuard::new()),
            rate_limiter: Arc::new(RateLimiter::new(RateLimitConfig::default())),
            secure_vault: Arc::new(SecureVault::new()),
            audit_log: Arc::new(AuditLog::new()),
        }
    }

    pub fn init_audit(&self, app_data_dir: &std::path::Path) {
        self.audit_log.init(app_data_dir);
    }

    pub fn issue_token(&self) -> Result<String> {
        let device_hash = self.fingerprint_store.compute_current_hash()?;
        self.token_manager.issue(&device_hash)
    }

    pub fn get_device_hash(&self) -> Result<String> {
        self.fingerprint_store.compute_current_hash()
    }

    pub fn get_usage_stats(&self) -> Option<crate::gateway::rate_limiter::DailyUsage> {
        let device_hash = self.fingerprint_store.compute_current_hash().ok()?;
        self.rate_limiter.get_usage_stats(&device_hash)
    }

    /// Primary Security Pipeline — Single Chokepoint for Outbound LLM Requests
    pub async fn process_request(
        &self,
        token: &str,
        request: CompletionRequest,
        provider: Arc<dyn LlmProvider>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        // Layer 1: Validate Ephemeral JWT Token
        let claims = match self.token_manager.validate(token) {
            Ok(c) => c,
            Err(e) => {
                self.audit_log.log_violation(
                    AuditEventType::TokenExpired,
                    "UNKNOWN",
                    &format!("Token validation failed: {}", e),
                );
                return Err(e);
            }
        };

        // Layer 2: Verify Device Fingerprint Hardware Binding
        if let Err(e) = self.fingerprint_store.verify(&claims.device_hash) {
            self.audit_log.log_violation(
                AuditEventType::DeviceMismatch,
                &claims.device_hash,
                "Device fingerprint mismatch detected! Possible token theft attempt.",
            );
            return Err(e);
        }

        // Layer 3: Check Scope & Intent Guard
        if let Err(e) = self.intent_guard.check_request(&request) {
            self.audit_log.log_violation(
                AuditEventType::ScopeViolation,
                &claims.device_hash,
                &format!("Scope violation: {}", e),
            );
            return Err(e);
        }

        // Layer 4: Token Bucket Rate Limiting & Daily Cap
        if let Err(e) = self.rate_limiter.check_and_consume(&claims.device_hash, &request) {
            let event = match e {
                AppError::QuotaExceeded(_) => AuditEventType::QuotaExceeded,
                _ => AuditEventType::RateLimitExceeded,
            };
            self.audit_log.log_violation(event, &claims.device_hash, &e.to_string());
            return Err(e);
        }

        // Layer 5: Execute the stream through the provider, tracking the actual
        // token usage reported by the final `usage` chunk so the daily budget /
        // usage stats reflect reality.
        let stream = self
            .secure_vault
            .execute_with_decrypted_key(provider, request)
            .await?;

        // Audit Log Success
        self.audit_log.log_success(&claims);

        // Tee the usage chunk into the rate limiter exactly once per request
        // (on the terminating `Done` chunk), using the last reported usage.
        let limiter = self.rate_limiter.clone();
        let device_hash = claims.device_hash.clone();
        let usage_state = std::sync::Arc::new(std::sync::Mutex::new(None::<StreamUsageCell>));
        let tracked_usage = usage_state.clone();
        let tracked = stream.map(move |item| {
            if let Ok(chunk) = &item {
                if let ChunkKind::Usage(u) = &chunk.kind {
                    *tracked_usage.lock().unwrap() = Some(StreamUsageCell {
                        input: u.input_tokens,
                        output: u.output_tokens,
                    });
                } else if matches!(chunk.kind, ChunkKind::Done) {
                    if let Some(usage) = tracked_usage.lock().unwrap().take() {
                        limiter.record_usage(&device_hash, usage.input, usage.output);
                    }
                }
            }
            item
        });

        Ok(Box::pin(tracked))
    }
}

#[derive(Clone, Copy)]
struct StreamUsageCell {
    input: u64,
    output: u64,
}

/// 32 random bytes hex-encoded, used as the in-process JWT signing secret.
fn random_secret() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Wraps a resolved LLM provider so EVERY streamed request passes through the
/// GatewayHub security pipeline (JWT validation, device fingerprint binding,
/// intent guard, rate limiting, audit log). This closes the "gateway is dead
/// code" gap: chat requests used to bypass every one of those checks.
///
/// A fresh token is minted per stream so a run paused at a long gate (or a
/// paused swarm phase) never hits the 30-minute token expiry mid-run.
pub struct GatewayProvider {
    pub inner: Arc<dyn LlmProvider>,
    pub gateway: Arc<GatewayHub>,
}

#[async_trait]
impl LlmProvider for GatewayProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn max_context_tokens(&self) -> usize {
        self.inner.max_context_tokens()
    }

    fn supports_tool_calling(&self) -> bool {
        self.inner.supports_tool_calling()
    }

    async fn stream_complete(
        &self,
        request: CompletionRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        let token = self.gateway.issue_token()?;
        self.gateway
            .process_request(&token, request, self.inner.clone())
            .await
    }

    async fn stream_complete_with_key(
        &self,
        _key: &str,
        request: CompletionRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        self.stream_complete(request).await
    }
}
