use std::collections::HashSet;

use crate::agent::llm_client::CompletionRequest;
use crate::error::{AppError, Result};

pub struct IntentGuard {
    required_context_keywords: Vec<String>,
    blocked_patterns: Vec<String>,
    max_payload_bytes: usize,
    allowed_tool_names: HashSet<String>,
}

impl IntentGuard {
    pub fn new() -> Self {
        Self {
            required_context_keywords: vec![
                "KudaIDE".to_string(),
                "code".to_string(),
                "refactor".to_string(),
                "developer".to_string(),
                "project_root".to_string(),
                "agent".to_string(),
            ],
            blocked_patterns: vec![
                "translate this article".to_string(),
                "write a blog post".to_string(),
                "generate marketing".to_string(),
                "bulk email".to_string(),
            ],
            max_payload_bytes: 512_000, // 500KB max payload
            allowed_tool_names: HashSet::new(),
        }
    }

    pub fn check_request(&self, request: &CompletionRequest) -> Result<()> {
        let serialized = serde_json::to_string(request)
            .map_err(|e| AppError::General(format!("Payload serialization error: {}", e)))?;

        if serialized.len() > self.max_payload_bytes {
            return Err(AppError::ScopeViolation(format!(
                "Payload size ({} bytes) exceeds 500KB limit",
                serialized.len()
            )));
        }

        let system_prompt = &request.system_prompt;
        let has_keyword = self
            .required_context_keywords
            .iter()
            .any(|kw| system_prompt.to_lowercase().contains(&kw.to_lowercase()));

        if !has_keyword && !system_prompt.is_empty() {
            return Err(AppError::ScopeViolation(
                "System prompt must contain valid KudaIDE context keyword".to_string(),
            ));
        }

        for msg in &request.messages {
            let content_lower = msg.content.to_lowercase();
            for pattern in &self.blocked_patterns {
                if content_lower.contains(pattern) {
                    return Err(AppError::ScopeViolation(format!(
                        "Blocked non-coding pattern detected: '{}'",
                        pattern
                    )));
                }
            }
        }

        Ok(())
    }

    pub fn add_allowed_tool(&mut self, name: &str) {
        self.allowed_tool_names.insert(name.to_string());
    }
}
