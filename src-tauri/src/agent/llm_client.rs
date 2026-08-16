use async_trait::async_trait;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use futures::Stream;
use crate::error::Result;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolCallChunk {
    pub call_id: String,
    pub tool_name: String,
    pub arguments_json: String,
}

/// Pemakaian token aktual yang dilaporkan provider/hub pada chunk akhir stream
/// (OpenAI `usage` / DeepSeek). Dipakai untuk menampilkan cache-hit di UI.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct StreamUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallChunk>>,
    /// For Tool role messages: the `call_id` of the assistant tool call this
    /// message responds to. OpenAI-compatible providers require the tool
    /// response's `tool_call_id` to match the assistant tool call's `id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Chain-of-thought emitted by reasoning models (e.g. DeepSeek
    /// `reasoning_content`). Must be passed back verbatim on the next request
    /// when the assistant message carries tool calls, or strict providers
    /// (vllm thinking mode) reject the conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Wall-clock time the message was produced (persisted in chat history for
    /// time-awareness). Never serialized into provider request bodies; when
    /// present it is stamped as a `[ISO8601]` prefix on user messages at
    /// request-build time. Missing on legacy history (backward compatible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Local>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ChunkKind {
    TextDelta(String),
    /// Chain-of-thought text from reasoning models (DeepSeek `reasoning_content`).
    ReasoningDelta(String),
    ToolCallStart(ToolCallChunk),
    ToolCallEnd,
    /// Token usage aktual dari provider/hub (context caching: `cached_input_tokens`).
    Usage(StreamUsage),
    Done,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StreamChunk {
    pub kind: ChunkKind,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters_schema: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CompletionRequest {
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub temperature: f32,
    /// Max output tokens. `None` = let the provider apply its own model max
    /// (a hard-coded 1M cap used to be sent and broke Gemini + strict
    /// OpenAI-compatible relays with a 400 INVALID_ARGUMENT).
    #[serde(default)]
    pub max_tokens: Option<usize>,
    pub model: String,
    #[serde(default)]
    pub tools: Vec<ToolSchema>,
}

impl Message {
    /// Returns a clone ready to be sent to the provider. User-role messages
    /// that carry a `created_at` get it stamped as a content prefix so the
    /// model is time-aware; all other messages pass through unchanged. The
    /// stored message itself is never mutated (history stays stable for
    /// prefix caching).
    pub fn stamped_for_request(&self) -> Self {
        let mut m = self.clone();
        if m.role == MessageRole::User {
            if let Some(ts) = m.created_at {
                m.content = format!("[{}] {}", ts.format("%Y-%m-%dT%H:%M%:z"), m.content);
            }
        }
        m
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            created_at: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            created_at: None,
        }
    }

    pub fn tool_result(tool_name: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: output.into(),
            name: Some(tool_name.into()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            created_at: None,
        }
    }

    /// Tool response that references the exact assistant tool `call_id` so
    /// strict OpenAI-compatible providers accept the conversation.
    pub fn tool_result_with_call_id(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        output: impl Into<String>,
    ) -> Self {
        Self {
            role: MessageRole::Tool,
            content: output.into(),
            name: Some(tool_name.into()),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
            reasoning_content: None,
            created_at: None,
        }
    }
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &str {
        self.name()
    }
    fn name(&self) -> &str;
    fn max_context_tokens(&self) -> usize;
    fn supports_tool_calling(&self) -> bool;

    async fn stream_complete(
        &self,
        request: CompletionRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>>;

    async fn stream_complete_with_key(
        &self,
        _key: &str,
        request: CompletionRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>>{
        self.stream_complete(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    #[test]
    fn test_stamped_for_request_prefixes_only_user_messages() {
        let ts = Local::now();
        let mut u = Message::user("hello");
        u.created_at = Some(ts);
        let stamped = u.stamped_for_request();
        assert!(
            stamped.content.starts_with('['),
            "user message must get a timestamp prefix: {}",
            stamped.content
        );
        assert!(stamped.content.ends_with("] hello"));

        // The stored message is untouched (history stays stable for caching).
        assert_eq!(u.content, "hello");

        // Assistant/tool messages pass through unchanged.
        assert_eq!(Message::assistant("hi").stamped_for_request().content, "hi");
        assert_eq!(
            Message::tool_result("t", "out").stamped_for_request().content,
            "out"
        );
    }
}
