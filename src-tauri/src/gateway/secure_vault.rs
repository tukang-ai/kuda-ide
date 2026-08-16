use std::pin::Pin;
use std::sync::Arc;
use futures::Stream;
use zeroize::Zeroize;

use crate::agent::key_store::KeyStore;
use crate::agent::llm_client::{CompletionRequest, LlmProvider, StreamChunk};
use crate::error::{AppError, Result};

pub struct ZeroizedKey {
    pub inner: String,
}

impl Drop for ZeroizedKey {
    fn drop(&mut self) {
        self.inner.zeroize();
    }
}

pub struct SecureVault;

impl SecureVault {
    pub fn new() -> Self {
        Self
    }

    pub fn get_key_zeroized(&self, provider_id: &str) -> Result<ZeroizedKey> {
        let key = KeyStore::get_api_key(provider_id)
            .map_err(|e| AppError::General(format!("KeyStore error: {}", e)))?;

        if key.trim().is_empty() {
            return Err(AppError::General(format!("No API Key stored for provider '{}'", provider_id)));
        }

        Ok(ZeroizedKey { inner: key })
    }

    pub async fn execute_with_decrypted_key(
        &self,
        provider: Arc<dyn LlmProvider>,
        request: CompletionRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        // The provider is built by `roles::build_provider` with its API key /
        // hub session key ALREADY baked in. Re-fetching a key here by
        // `provider.id()` would look up the wrong keychain entry (id() returns
        // the MODEL name, not the keychain service name like "provider_key.x"),
        // so delegation is both correct and the only reliable path.
        provider.stream_complete(request).await
    }

    pub fn has_key(&self, provider_id: &str) -> bool {
        KeyStore::get_api_key(provider_id).is_ok()
    }
}
