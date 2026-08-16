use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use uuid::Uuid;

use crate::error::{AppError, Result};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TokenClaims {
    pub sub: String,         // "kuda-ide-gateway"
    pub device_hash: String, // SHA-256 hash dari device fingerprint
    pub scope: String,       // "coding"
    pub iat: i64,            // Issued At timestamp
    pub exp: i64,            // Expiration timestamp
    pub jti: String,         // Unique token ID
}

pub struct EphemeralTokenManager {
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
    token_lifetime: Duration,
    revoked_tokens: Mutex<HashSet<String>>,
    active_tokens: Mutex<HashMap<String, i64>>,
}

impl EphemeralTokenManager {
    pub fn new(jwt_secret: &str) -> Self {
        Self {
            encoding_key: EncodingKey::from_secret(jwt_secret.as_bytes()),
            decoding_key: DecodingKey::from_secret(jwt_secret.as_bytes()),
            token_lifetime: Duration::minutes(30),
            revoked_tokens: Mutex::new(HashSet::new()),
            active_tokens: Mutex::new(HashMap::new()),
        }
    }

    pub fn issue(&self, device_hash: &str) -> Result<String> {
        let now = Utc::now();
        let exp = (now + self.token_lifetime).timestamp();
        let jti = Uuid::new_v4().to_string();

        let claims = TokenClaims {
            sub: "kuda-ide-gateway".to_string(),
            device_hash: device_hash.to_string(),
            scope: "coding".to_string(),
            iat: now.timestamp(),
            exp,
            jti: jti.clone(),
        };

        if let Ok(mut active) = self.active_tokens.lock() {
            // Tokens are now minted per stream request, so prune expired
            // entries when the map grows to keep it bounded.
            if active.len() > 4096 {
                let now = Utc::now().timestamp();
                active.retain(|_, exp| *exp > now);
            }
            active.insert(jti, exp);
        }

        encode(&Header::default(), &claims, &self.encoding_key)
            .map_err(|e| AppError::TokenInvalid(format!("Failed to issue JWT: {}", e)))
    }

    pub fn validate(&self, token: &str) -> Result<TokenClaims> {
        let token_data = decode::<TokenClaims>(
            token,
            &self.decoding_key,
            &Validation::default(),
        )
        .map_err(|e| AppError::TokenInvalid(format!("Invalid or expired JWT: {}", e)))?;

        let claims = token_data.claims;

        if let Ok(revoked) = self.revoked_tokens.lock() {
            if revoked.contains(&claims.jti) {
                return Err(AppError::TokenInvalid("Token has been revoked".to_string()));
            }
        }

        if claims.scope != "coding" {
            return Err(AppError::ScopeViolation(format!(
                "Invalid token scope '{}', expected 'coding'",
                claims.scope
            )));
        }

        Ok(claims)
    }

    pub fn revoke(&self, jti: &str) {
        if let Ok(mut revoked) = self.revoked_tokens.lock() {
            revoked.insert(jti.to_string());
        }
    }

    pub fn cleanup_expired(&self) {
        let now = Utc::now().timestamp();
        if let Ok(mut active) = self.active_tokens.lock() {
            active.retain(|_, exp| *exp > now);
        }
        if let Ok(mut revoked) = self.revoked_tokens.lock() {
            let active_set: HashSet<String> = self
                .active_tokens
                .lock()
                .map(|a| a.keys().cloned().collect())
                .unwrap_or_default();
            revoked.retain(|jti| active_set.contains(jti));
        }
    }
}
