use keyring::Entry;
use crate::error::{AppError, Result};

const SERVICE_NAME: &str = "com.kuda.ide.credentials";

pub struct KeyStore;

impl KeyStore {
    /// Saves LLM API Key (e.g. Gemini/Claude) into OS Keychain
    pub fn save_api_key(provider: &str, api_key: &str) -> Result<()> {
        let entry = Entry::new(SERVICE_NAME, provider)
            .map_err(|e| AppError::General(format!("Failed to access Keychain entry: {}", e)))?;
        
        entry
            .set_password(api_key)
            .map_err(|e| AppError::General(format!("Failed to store API Key in Keychain: {}", e)))?;
        
        tracing::info!("Successfully stored API Key for provider '{}' in OS Keychain", provider);
        Ok(())
    }

    /// Deletes a stored credential from the OS Keychain (used to clear config fields).
    pub fn delete_api_key(provider: &str) -> Result<()> {
        let entry = Entry::new(SERVICE_NAME, provider)
            .map_err(|e| AppError::General(format!("Failed to access Keychain entry: {}", e)))?;
        entry
            .delete_credential()
            .map_err(|e| AppError::General(format!("Failed to delete Keychain entry '{}': {}", provider, e)))?;
        tracing::info!("Deleted Keychain entry for '{}'", provider);
        Ok(())
    }

    /// Retrieves LLM API Key from OS Keychain, falling back to Environment Variables
    pub fn get_api_key(provider: &str) -> Result<String> {
        // 1. Try OS Keychain
        if let Ok(entry) = Entry::new(SERVICE_NAME, provider) {
            if let Ok(key) = entry.get_password() {
                if !key.trim().is_empty() {
                    return Ok(key);
                }
            }
        }

        // 2. Fallback to Environment Variable (GEMINI_API_KEY / ANTHROPIC_API_KEY)
        let env_var_name = match provider.to_lowercase().as_str() {
            "gemini" => "GEMINI_API_KEY",
            "claude" | "anthropic" => "ANTHROPIC_API_KEY",
            "openai" => "OPENAI_API_KEY",
            _ => "LLM_API_KEY",
        };

        if let Ok(env_key) = std::env::var(env_var_name) {
            if !env_key.trim().is_empty() {
                return Ok(env_key);
            }
        }

        Err(AppError::General(format!(
            "API Key not found for provider '{}' in OS Keychain or environment variable '{}'",
            provider, env_var_name
        )))
    }

    /// Retrieves a value from the OS Keychain ONLY — never the environment
    /// variable fallback. Used for NON-secret config values (model names, base
    /// URLs) that are stored as keychain entries: letting them fall through to
    /// `LLM_API_KEY` (the catch-all env var for unknown keys) could silently
    /// substitute an API key as a model name / base URL.
    pub fn get_api_key_from_keychain(provider: &str) -> Result<String> {
        let entry = Entry::new(SERVICE_NAME, provider)
            .map_err(|e| AppError::General(format!("Failed to access Keychain entry: {}", e)))?;
        let key = entry
            .get_password()
            .map_err(|e| AppError::General(format!("Keychain entry '{}' not found: {}", provider, e)))?;
        if key.trim().is_empty() {
            return Err(AppError::General(format!("Keychain entry '{}' is empty", provider)));
        }
        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fallback_to_env_var() {
        std::env::set_var("GEMINI_API_KEY", "test_gemini_key_123");
        let key = KeyStore::get_api_key("gemini");
        assert!(key.is_ok());
        assert_eq!(key.unwrap(), "test_gemini_key_123");
    }
}
