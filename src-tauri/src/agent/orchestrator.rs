use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::Arc;
use tauri::ipc::Channel;
use crate::error::{AppError, Result};
use crate::agent::llm_client::{ChunkKind, CompletionRequest, LlmProvider, Message, StreamChunk, StreamUsage, ToolSchema};
use crate::agent::prompt_composer::PromptComposer;
use crate::agent::tokenizer::Tokenizer;
use crate::agent::tool_registry::{ToolContext, ToolRegistry};

const MAX_AGENT_TURNS: usize = 12;
/// Transient LLM errors (rate limit, 5xx, connection reset) are retried with
/// exponential backoff (respecting a provider-supplied "try again in N seconds"
/// hint when present) before the whole run is abandoned. Permanent errors
/// (auth, schema, 400s) are NOT retried so failed calls are never multiplied.
const MAX_LLM_STREAM_RETRIES: usize = 10;
/// Soft cap on total characters of tool outputs retained in history, so a
/// long tool loop cannot blow past the provider's context window. Generous by
/// design: the RLM Model must be able to SEE full file regions to copy them
/// verbatim into the brief (completeness > token economy for research).
const MAX_TOOL_OUTPUT_TOTAL_CHARS: usize = 120_000;

/// Classifies an LLM failure as transient (safe + worthwhile to retry):
/// rate limits (429), server errors (5xx), connection resets/timeouts, and
/// upstream "try again" hints. Everything else (auth, schema, 400s) is
/// permanent — retrying it only multiplies failed LLM calls.
fn is_transient_llm_error(err: &AppError) -> bool {
    if matches!(err, AppError::RateLimitExceeded(_)) {
        return true;
    }
    // Upstream status codes: 401 (expired/rotated hub key) is retried WITH a
    // forced credential refresh, 408/409/429/5xx are plain transient retries.
    if let AppError::Api { status, .. } = err {
        return *status == 401
            || *status == 408
            || *status == 409
            || *status == 429
            || *status >= 500;
    }
    let AppError::General(msg) = err else {
        return false;
    };
    let msg = msg.to_lowercase();
    // A non-retryable 4xx status code in the text is a permanent error even if
    // the message body happens to mention "timeout" / "429" / "500" (e.g. a 400
    // "parameter 'timeout' is invalid"). Refuse before the substring heuristics
    // below can misfire.
    for code in ["400", "401", "403", "404", "405", "406", "413", "415", "422"] {
        if msg.contains(code) {
            return false;
        }
    }
    ["429", "500", "501", "502", "503", "504"]
        .iter()
        .any(|c| msg.contains(c))
        || msg.contains("rate limit")
        || msg.contains("too many requests")
        || msg.contains("try again")
        || msg.contains("timed out")
        || msg.contains("timeout")
        || msg.contains("connection reset")
        || msg.contains("connection refused")
        || msg.contains("connection closed")
        || msg.contains("service unavailable")
        || msg.contains("bad gateway")
        || msg.contains("no deployments available")
        || msg.contains("transport error")
        || msg.contains("error decoding response body")
        || msg.contains("sse error")
        || msg.contains("stream error")
        || msg.contains("broken pipe")
        || msg.contains("origin-close")
}

/// Extracts a provider-suggested retry delay ("Try again in 5 seconds" or
/// "Retry-After: 30"), clamped to a sane upper bound. Returns `None` when the
/// message carries no hint, so the caller falls back to its own backoff.
fn parse_retry_after_seconds(err: &AppError) -> Option<u64> {
    let msg = err.to_string().to_lowercase();
    for pat in ["try again in ", "retry-after: ", "try again in about "] {
        if let Some(idx) = msg.find(pat) {
            let rest = &msg[idx + pat.len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse::<u64>() {
                if (1..=120).contains(&n) {
                    return Some(n);
                }
            }
        }
    }
    None
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum AgentEventKind {
    ThoughtDelta(String),
    /// Reasoning/thinking deltas streamed to the UI so the user sees the agent
    /// "thinking" live instead of a blank bubble during long reasoning phases.
    ThinkingDelta(String),
    ToolCallStarted { tool_name: String, call_id: String, arguments_json: String },
    ToolCallCompleted { tool_name: String, call_id: String, output: String },
    /// Run totals. `tokens_in`/`tokens_out` are cumulative across every role in
    /// the run; `total_tokens_used` is kept as the sum for backward compat.
    Finished {
        total_tokens_used: usize,
        tokens_in: usize,
        tokens_out: usize,
        cached_in: usize,
    },
    Error(String),
    PhaseStarted { role: String, label: String, model: String },
    /// Completing ONE role phase. `tokens_in`/`tokens_out` are THAT role's own
    /// estimated usage (different agents use different models with different
    /// prices, so per-role counts must be shown separately, not the run total).
    PhaseCompleted {
        role: String,
        summary: String,
        tokens_in: usize,
        tokens_out: usize,
        cached_in: usize,
    },
    /// Emitted by `request_external_access` for each out-of-project path the
    /// RLM Model wants to read. Frontend shows an Allow/Deny popup per request.
    ExternalAccessRequest {
        request_id: String,
        path: String,
        reason: String,
        kind: String,
    },
    /// Emitted after the user resolves an external access request.
    ExternalAccessResolved { request_id: String, allowed: bool },
    /// Emitted by the Plan Approval Gate after the Thinker produces a plan.
    /// The run pauses until the user resolves it via
    /// `agent_resolve_plan_decision` ("execute" | "revise" | "review").
    PlanDecisionRequest {
        request_id: String,
        plan_markdown: String,
        /// Project-relative path where the plan artifact lives (e.g.
        /// ".kuda/plan.md") so the UI can open it in the editor.
        plan_file_path: String,
        round: usize,
        tasks_count: usize,
        latest_note: Option<String>,
    },
    /// Emitted after the user resolves a plan decision request.
    PlanDecisionResolved {
        request_id: String,
        decision: String,
        note: Option<String>,
    },
    /// Emitted by the Thinker-direction checkpoint BEFORE the full plan is
    /// made: the Thinker's temporary conclusion is shown for a brief user
    /// review. The run pauses until the user resolves it via
    /// `agent_resolve_direction_decision` ("lanjut" | "ubah").
    DirectionDecisionRequest {
        request_id: String,
        conclusion: String,
    },
    /// Emitted after the user resolves a direction decision request.
    DirectionDecisionResolved {
        request_id: String,
        decision: String,
        note: Option<String>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentEvent {
    pub kind: AgentEventKind,
}

pub struct AgentOrchestrator {
    tool_registry: Arc<ToolRegistry>,
}

impl AgentOrchestrator {
    pub fn new(tool_registry: Arc<ToolRegistry>) -> Self {
        Self { tool_registry }
    }

    /// Calls `provider.stream_complete` with retries on transient errors only.
    ///
    /// Retryable failures: 429 rate limits, 5xx, connection resets/timeouts and
    /// upstream "try again" hints. The backoff waits for the provider's own
    /// `Retry-After` / "Try again in N seconds" hint when present, otherwise it
    /// doubles from 1s (1s, 2s, 4s, 8s) plus a little jitter — so a 429 is
    /// never hammered with rapid-fire duplicate calls, which is what actually
    /// makes the provider's cooldown worse.
    ///
    /// The INITIAL request POST is wrapped in `select!` against the run's
    /// cancel flag, so a base URL that accepts the connection but never
    /// responds (tarpit) can be aborted by `agent_cancel_run` instead of
    /// hanging the run forever (the provider clients also carry their own
    /// connect/read timeouts as a second backstop).
    async fn stream_with_retry(
        &self,
        provider: &Arc<dyn LlmProvider>,
        request: &CompletionRequest,
        app_data_dir: &std::path::Path,
        cancel: &crate::agent::tool_registry::CancelFlag,
    ) -> crate::error::Result<Pin<Box<dyn Stream<Item = crate::error::Result<StreamChunk>> + Send>>> {
        for attempt in 0..MAX_LLM_STREAM_RETRIES {
            if cancel.is_cancelled() {
                return Err(AppError::General("Run cancelled by user.".to_string()));
            }
            let stream = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return Err(AppError::General("Run cancelled by user.".to_string()));
                }
                r = provider.stream_complete(request.clone()) => r,
            };
            if cancel.is_cancelled() {
                return Err(AppError::General("Run cancelled by user.".to_string()));
            }
            match stream {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    if !is_transient_llm_error(&e) {
                        // Permanent failure: do not burn retries / make extra calls.
                        return Err(e);
                    }
                    // Auth failures: the hub session key may have been rotated or
                    // expired server-side while a run was paused (e.g. at a gate).
                    // Force a refresh so the next attempt uses a live key.
                    if matches!(&e, AppError::Api { status: 401, .. }) {
                        let _ =
                            crate::agent::hub_session::refresh_hub_session(app_data_dir).await;
                    }
                    if attempt + 1 == MAX_LLM_STREAM_RETRIES {
                        return Err(e);
                    }
                    let base_ms = parse_retry_after_seconds(&e)
                        .map(|s| s * 1000)
                        .unwrap_or_else(|| 1000u64 << attempt.min(4));
                    let jitter = rand::random::<u64>() % 500;
                    tracing::warn!(
                        "Transient LLM error ({}); retrying in {} ms (attempt {}/{})",
                        e,
                        base_ms + jitter,
                        attempt + 2,
                        MAX_LLM_STREAM_RETRIES
                    );
                    // The backoff sleep is also cancellable.
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            return Err(AppError::General("Run cancelled by user.".to_string()));
                        }
                        _ = tokio::time::sleep(std::time::Duration::from_millis(base_ms + jitter)) => {}
                    }
                }
            }
        }
        unreachable!("MAX_LLM_STREAM_RETRIES >= 1 always returns from the loop")
    }

    /// Restarts the current turn's request when the stream dies mid-flight
    /// BEFORE any output was delivered (nothing streamed to the UI yet, no tool
    /// calls started). Returns `None` when a restart is not allowed/safe, so the
    /// caller propagates the original error instead.
    async fn retry_after_empty_transient_stream(
        &self,
        err: &AppError,
        emitted_any: bool,
        restarts_left: &mut usize,
        cancel: &crate::agent::tool_registry::CancelFlag,
    ) -> Option<()> {
        if *restarts_left == 0
            // HONOR the flag: once ANY text/tool-call delta reached the app,
            // replaying the turn would re-execute side effects (duplicate file
            // edits, duplicated UI text). Only a fully-empty attempt may be
            // transparently restarted.
            || emitted_any
            || !is_transient_llm_error(err)
            || cancel.is_cancelled()
        {
            return None;
        }
        *restarts_left -= 1;
        let base_ms = parse_retry_after_seconds(err)
            .map(|s| s * 1000)
            .unwrap_or(2000);
        let jitter = rand::random::<u64>() % 500;
        tracing::warn!(
            "LLM stream encountered transient network error ({}); restarting turn in {} ms ({} restart(s) left)",
            err,
            base_ms + jitter,
            *restarts_left + 1
        );
        // Cancellable sleep: `agent_cancel_run` must interrupt the restart wait
        // instead of letting the run hang for the full backoff.
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            _ = tokio::time::sleep(std::time::Duration::from_millis(base_ms + jitter)) => {}
        }
        Some(())
    }

    /// Truncates a tool output so the running total of retained tool output
    /// stays under the context budget. Always keeps at least 64 chars so the
    /// model still receives a useful signal.
    fn bounded_truncate_middle(&self, output: &str, retained_chars: usize) -> String {
        let cap = 24000usize.min(retained_chars.max(64));
        truncate_middle(output, cap)
    }

    /// Runs the multi-turn agentic loop:
    /// streams the model response, executes any requested tools, feeds results
    /// back to the model, and repeats until the model produces a final answer
    /// or MAX_AGENT_TURNS is reached. Every step is streamed over the Channel.
    ///
    /// Returns the complete message trace of this run (starting with the user
    /// message) so callers can persist it into chat history.
    pub async fn run_loop(
        &self,
        messages: &[Message],
        provider: Arc<dyn LlmProvider>,
        tool_ctx: &ToolContext,
        auto_approve: bool,
        on_event: &Channel<AgentEvent>,
    ) -> std::result::Result<Vec<Message>, RunFailure> {
        let system_prompt = PromptComposer::compose_system_prompt(&tool_ctx.project_root);
        // Handoff tools (submit_plan / submit_plan_review / submit_verdict /
        // submit_audit / submit_brief / submit_review_directions /
        // request_rlm_research) are swarm-only no-ops; exposing them in direct
        // chat only confuses the model into reporting work that never happened.
        let tools: Vec<ToolSchema> = self
            .tool_registry
            .get_definitions()
            .into_iter()
            .filter(|d| {
                !matches!(
                    d.name.as_str(),
                    "submit_plan"
                        | "submit_plan_review"
                        | "submit_verdict"
                        | "submit_audit"
                        | "submit_brief"
                        | "submit_review_directions"
                        | "request_rlm_research"
                        | "call_rlm_research"
                        | "call_thinker_direction"
                        | "call_planning_swarm"
                        | "call_executor"
                )
            })
            .map(|d| ToolSchema {
                name: d.name,
                description: d.description,
                parameters_schema: d.parameters_schema,
            })
            .collect();

        let mut history: Vec<Message> = messages.to_vec();
        let mut run_trace: Vec<Message> = Vec::new();
        let mut input_tokens: usize = 0;
        let mut output_tokens: usize = 0;
        let mut cached_in: usize = 0;
        let mut tool_output_chars: usize = 0;

        let emit = |kind: AgentEventKind| {
            // If the UI channel is gone (panel closed), treat it as cancellation.
            if on_event.send(AgentEvent { kind }).is_err() {
                tool_ctx.cancel.cancel();
            }
        };

        for _turn in 0..MAX_AGENT_TURNS {
            if tool_ctx.cancel.is_cancelled() {
                emit(AgentEventKind::Error("Run cancelled by user.".to_string()));
                return Err(RunFailure {
                    error: AppError::General("Run cancelled by user.".to_string()),
                    partial_trace: run_trace.clone(),
                });
            }

            let request = CompletionRequest {
                system_prompt: system_prompt.clone(),
                messages: history.iter().map(Message::stamped_for_request).collect(),
                temperature: 0.2,
                // Output cap left unset: a hard-coded 1M used to be sent and
                // caused 400 INVALID_ARGUMENT on Gemini (maxOutputTokens over the
                // model cap) and on strict OpenAI-compatible relays. Omitted, the
                // providers use their own model max, so plans are never rejected.
                max_tokens: None,
                model: provider.name().to_string(),
                tools: tools.clone(),
            };

            let (text_buffer, reasoning_buffer, tool_calls, turn_usage) = {
                let mut stream_restarts = 2usize;
                loop {
                let mut stream = match self
                    .stream_with_retry(&provider, &request, &tool_ctx.app_data_dir, &tool_ctx.cancel)
                    .await
                {
                        Ok(stream) => stream,
                        Err(e) => {
                            emit(AgentEventKind::Error(e.to_string()));
                            return Err(RunFailure {
                                error: e,
                                partial_trace: run_trace.clone(),
                            });
                        }
                    };

                    let mut text_buffer = String::new();
                    let mut reasoning_buffer = String::new();
                    let mut tool_calls: Vec<crate::agent::llm_client::ToolCallChunk> = Vec::new();
                    let mut transient_restart = false;
                    let mut turn_usage: Option<StreamUsage> = None;

                    loop {
                        if tool_ctx.cancel.is_cancelled() {
                            emit(AgentEventKind::Error("Run cancelled by user.".to_string()));
                            return Err(RunFailure {
                                error: AppError::General("Run cancelled by user.".to_string()),
                                partial_trace: run_trace.clone(),
                            });
                        }
                        tokio::select! {
                            chunk = stream.next() => {
                                let Some(chunk_result) = chunk else { break };
                                match chunk_result {
                                    Ok(chunk) => match chunk.kind {
                                        ChunkKind::TextDelta(delta) => {
                                            if !delta.is_empty() {
                                                text_buffer.push_str(&delta);
                                                emit(AgentEventKind::ThoughtDelta(delta));
                                            }
                                        }
                                        ChunkKind::ReasoningDelta(delta) => {
                                            reasoning_buffer.push_str(&delta);
                                            if !delta.is_empty() {
                                                emit(AgentEventKind::ThinkingDelta(delta));
                                            }
                                        }
                                        ChunkKind::ToolCallStart(tc) => {
                                            tool_calls.push(tc);
                                        }
                                        ChunkKind::Usage(u) => {
                                            turn_usage = Some(u);
                                        }
                                        ChunkKind::ToolCallEnd | ChunkKind::Done => {}
                                    },
                                    Err(e) => {
                                        let emitted_any =
                                            !text_buffer.is_empty() || !tool_calls.is_empty();
                                        if self
                                            .retry_after_empty_transient_stream(
                                                &e,
                                                emitted_any,
                                                &mut stream_restarts,
                                                &tool_ctx.cancel,
                                            )
                                            .await
                                            .is_some()
                                        {
                                            transient_restart = true;
                                            break;
                                        }
                                        if tool_ctx.cancel.is_cancelled() {
                                            emit(AgentEventKind::Error(
                                                "Run cancelled by user.".to_string(),
                                            ));
                                            return Err(RunFailure {
                                                error: AppError::General(
                                                    "Run cancelled by user.".to_string(),
                                                ),
                                                partial_trace: run_trace.clone(),
                                            });
                                        }
                                        emit(AgentEventKind::Error(e.to_string()));
                                        return Err(RunFailure {
                                            error: e,
                                            partial_trace: run_trace.clone(),
                                        });
                                    }
                                }
                            }
                            _ = tool_ctx.cancel.cancelled() => {}
                        }
                    }

                    if transient_restart {
                        continue;
                    }
                    break (text_buffer, reasoning_buffer, tool_calls, turn_usage);
                }
            };

            match turn_usage {
                Some(u) => {
                    input_tokens += u.input_tokens as usize;
                    output_tokens += u.output_tokens as usize;
                    cached_in += u.cached_input_tokens as usize;
                }
                None => {
                    input_tokens += Tokenizer::count_tokens(&system_prompt)
                        + history
                            .iter()
                            .map(|m| Tokenizer::count_tokens(&m.content))
                            .sum::<usize>();
                    output_tokens +=
                        Tokenizer::count_tokens(&text_buffer) + Tokenizer::count_tokens(&reasoning_buffer);
                }
            }

            if tool_ctx.cancel.is_cancelled() {
                emit(AgentEventKind::Error("Run cancelled by user.".to_string()));
                return Err(RunFailure {
                    error: AppError::General("Run cancelled by user.".to_string()),
                    partial_trace: run_trace.clone(),
                });
            }

            if tool_calls.is_empty() {
                let mut final_message = Message::assistant(&text_buffer);
                if !reasoning_buffer.is_empty() {
                    final_message.reasoning_content = Some(reasoning_buffer);
                }
                run_trace.push(final_message.clone());
                emit(AgentEventKind::Finished {
                    total_tokens_used: input_tokens + output_tokens,
                    tokens_in: input_tokens,
                    tokens_out: output_tokens,
                    cached_in,
                });
                return Ok(run_trace);
            }

            // Model turn with tool calls
            let mut assistant_message = Message::assistant(text_buffer.clone());
            if !reasoning_buffer.is_empty() {
                assistant_message.reasoning_content = Some(reasoning_buffer.clone());
            }
            assistant_message.tool_calls = Some(tool_calls.clone());
            history.push(assistant_message.clone());
            run_trace.push(assistant_message);

            // Execute each requested tool
            for call in &tool_calls {
                if tool_ctx.cancel.is_cancelled() {
                    emit(AgentEventKind::Error("Run cancelled by user.".to_string()));
                    return Err(RunFailure {
                        error: AppError::General("Run cancelled by user.".to_string()),
                        partial_trace: run_trace.clone(),
                    });
                }
                let result = self
                    .run_tool_call(call, tool_ctx, auto_approve, None, &emit)
                    .await;

                let truncated_result = crate::agent::tool_registry::ToolResult {
                    success: result.success,
                    output: self.bounded_truncate_middle(
                        &result.output,
                        MAX_TOOL_OUTPUT_TOTAL_CHARS.saturating_sub(tool_output_chars),
                    ),
                    is_error: result.is_error,
                };
                tool_output_chars = tool_output_chars.saturating_add(truncated_result.output.len());
                let tool_message = Message::tool_result_with_call_id(
                    &call.call_id,
                    &call.tool_name,
                    serde_json::to_string(&truncated_result).unwrap_or_else(|_| result.output.clone()),
                );
                history.push(tool_message.clone());
                run_trace.push(tool_message);
            }
        }

        emit(AgentEventKind::Error(format!(
            "Agent reached maximum turn limit ({}), stopping.",
            MAX_AGENT_TURNS
        )));
        emit(AgentEventKind::Finished {
            total_tokens_used: input_tokens + output_tokens,
            tokens_in: input_tokens,
            tokens_out: output_tokens,
            cached_in,
        });
        Ok(run_trace)
    }
}

/// Error returned by `run_loop` when the run aborts mid-way. Carries the
/// partial message trace produced before the failure so the caller can persist
/// it — a failed run must still leave its conversation in history instead of
/// only the user's prompt.
pub struct RunFailure {
    pub error: AppError,
    pub partial_trace: Vec<Message>,
}

/// Outcome of one role-scoped agentic loop.
pub struct RoleLoopOutcome {
    /// Complete message history of this role's conversation (input messages +
    /// every assistant/tool message produced during the loop). The caller can
    /// append this back into the shared swarm context.
    pub history: Vec<Message>,
    /// Text of the final assistant message when the loop ended without tool calls.
    pub final_text: String,
    /// Name of the stop tool invoked by the model when the loop stopped on a tool.
    pub stop_tool_name: Option<String>,
    /// arguments_json of the stop tool (e.g. submit_plan / submit_verdict) when
    /// the model invoked it; None otherwise.
    pub stop_tool_args: Option<String>,
    /// INPUT tokens for this role loop as reported by the provider's stream
    /// `usage`, summed per request (local estimate when the provider sends none).
    pub tokens_in: usize,
    /// OUTPUT tokens generated by the model, from the provider's stream `usage`.
    pub tokens_out: usize,
    /// Cached input tokens reported by the provider (context caching). 0 when
    /// the upstream does not report `usage`.
    pub cached_in: usize,
    /// Total (in + out), kept for existing callers that only need a single number.
    pub tokens_used: usize,
    /// True when the loop ran out of turns with the model still issuing tools and
    /// never calling the stop tool (so `final_text` is the text of an unfinished
    /// tool-calling turn, not a real answer). Callers must not present a
    /// turn-exhausted outcome as a clean "answered directly".
    pub exhausted_turns: bool,
}

/// Per-role parameters for `run_role_loop`, grouped so the call sites stay
/// readable and the signature does not grow with every new knob.
pub struct RoleLoopParams<'a> {
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub allowed_tools: &'a [String],
    pub provider: Arc<dyn LlmProvider>,
    pub max_turns: usize,
    pub temperature: f32,
    /// Name of the tool whose invocation concludes the loop (submit_plan /
    /// submit_audit / submit_verdict / submit_brief). Can be pipe-separated
    /// (e.g. "tool_a|tool_b") to intercept any matching tool. None for roles
    /// that end with a plain text answer.
    pub stop_on_tool: Option<&'a str>,
    /// Display key of the role running the loop (used in error messages).
    pub role_name: &'a str,
}

impl AgentOrchestrator {
    /// Role-scoped agentic loop used by the swarm pipeline.
    ///
    /// Differences vs `run_loop`:
    /// - system prompt, allowed tools, turn limit and temperature are injected
    ///   by the caller per role;
    /// - when the model calls `stop_on_tool` (submit_plan / submit_verdict),
    ///   the loop captures its arguments and stops instead of executing it;
    /// - does not emit `Finished` (the swarm emits it once for the whole run).
    pub async fn run_role_loop(
        &self,
        params: RoleLoopParams<'_>,
        tool_ctx: &ToolContext,
        auto_approve: bool,
        emit: &impl Fn(AgentEventKind),
    ) -> Result<RoleLoopOutcome> {
        let tools: Vec<ToolSchema> = self
            .tool_registry
            .get_definitions_filtered(params.allowed_tools)
            .into_iter()
            .map(|d| ToolSchema {
                name: d.name,
                description: d.description,
                parameters_schema: d.parameters_schema,
            })
            .collect();

        let mut history: Vec<Message> = params.messages;
        let mut input_tokens: usize = 0;
        let mut output_tokens: usize = 0;
        let mut cached_in: usize = 0;

        let mut last_text = String::new();
        let mut tool_output_chars: usize = 0;

        for turn in 0..params.max_turns {
            if tool_ctx.cancel.is_cancelled() {
                emit(AgentEventKind::Error("Run cancelled by user.".to_string()));
                return Err(AppError::General("Run cancelled by user.".to_string()));
            }

            // Soft turn limit: on the final turn the model is nudged to conclude
            // instead of spending the last turn on another tool call. The nudge is
            // only sent on this request (never stored in `history`), so the happy
            // path (role finishing early) is completely unaffected.
            let mut request_messages: Vec<Message> =
                history.iter().map(Message::stamped_for_request).collect();
            let is_last_turn = turn + 1 == params.max_turns;
            if is_last_turn && !tools.is_empty() {
                request_messages.push(Message::user(last_turn_nudge(params.stop_on_tool)));
            }

            let request = CompletionRequest {
                system_prompt: params.system_prompt.clone(),
                messages: request_messages,
                temperature: params.temperature,
                // Output cap left unset — see `run_loop`: a hard-coded 1M
                // max_tokens broke Gemini and strict relays with a 400.
                max_tokens: None,
                model: params.provider.name().to_string(),
                tools: tools.clone(),
            };

            let (text_buffer, reasoning_buffer, tool_calls, turn_usage) = {
                let mut stream_restarts = 2usize;
                loop {
                    let mut stream = self
                        .stream_with_retry(
                            &params.provider,
                            &request,
                            &tool_ctx.app_data_dir,
                            &tool_ctx.cancel,
                        )
                        .await
                        .map_err(|e| {
                            emit(AgentEventKind::Error(format!("[{}] {}", params.provider.name(), e)));
                            e
                        })?;

                    let mut text_buffer = String::new();
                    let mut reasoning_buffer = String::new();
                    let mut tool_calls: Vec<crate::agent::llm_client::ToolCallChunk> = Vec::new();
                    let mut transient_restart = false;
                    let mut turn_usage: Option<StreamUsage> = None;

                    loop {
                        if tool_ctx.cancel.is_cancelled() {
                            emit(AgentEventKind::Error("Run cancelled by user.".to_string()));
                            return Err(AppError::General("Run cancelled by user.".to_string()));
                        }
                        tokio::select! {
                            chunk = stream.next() => {
                                let Some(chunk_result) = chunk else { break };
                                match chunk_result {
                                    Ok(chunk) => match chunk.kind {
                                        ChunkKind::TextDelta(delta) => {
                                            if !delta.is_empty() {
                                                text_buffer.push_str(&delta);
                                                emit(AgentEventKind::ThoughtDelta(delta));
                                            }
                                        }
                                        ChunkKind::ReasoningDelta(delta) => {
                                            reasoning_buffer.push_str(&delta);
                                            if !delta.is_empty() {
                                                emit(AgentEventKind::ThinkingDelta(delta));
                                            }
                                        }
                                        ChunkKind::ToolCallStart(tc) => {
                                            tool_calls.push(tc);
                                        }
                                        ChunkKind::Usage(u) => {
                                            turn_usage = Some(u);
                                        }
                                        ChunkKind::ToolCallEnd | ChunkKind::Done => {}
                                    },
                                    Err(e) => {
                                        let emitted_any =
                                            !text_buffer.is_empty() || !tool_calls.is_empty();
                                        if self
                                            .retry_after_empty_transient_stream(
                                                &e,
                                                emitted_any,
                                                &mut stream_restarts,
                                                &tool_ctx.cancel,
                                            )
                                            .await
                                            .is_some()
                                        {
                                            transient_restart = true;
                                            break;
                                        }
                                        if tool_ctx.cancel.is_cancelled() {
                                            emit(AgentEventKind::Error(
                                                "Run cancelled by user.".to_string(),
                                            ));
                                            return Err(AppError::General(
                                                "Run cancelled by user.".to_string(),
                                            ));
                                        }
                                        emit(AgentEventKind::Error(e.to_string()));
                                        return Err(e);
                                    }
                                }
                            }
                            _ = tool_ctx.cancel.cancelled() => {}
                        }
                    }

                    if transient_restart {
                        continue;
                    }
                    break (text_buffer, reasoning_buffer, tool_calls, turn_usage);
                }
            };

            match turn_usage {
                Some(u) => {
                    input_tokens += u.input_tokens as usize;
                    output_tokens += u.output_tokens as usize;
                    cached_in += u.cached_input_tokens as usize;
                }
                None => {
                    input_tokens += Tokenizer::count_tokens(&params.system_prompt)
                        + history
                            .iter()
                            .map(|m| Tokenizer::count_tokens(&m.content))
                            .sum::<usize>();
                    output_tokens +=
                        Tokenizer::count_tokens(&text_buffer) + Tokenizer::count_tokens(&reasoning_buffer);
                }
            }

            if tool_ctx.cancel.is_cancelled() {
                emit(AgentEventKind::Error("Run cancelled by user.".to_string()));
                return Err(AppError::General("Run cancelled by user.".to_string()));
            }

            // Intercept structured-handoff tool before anything else. Supports pipe-separated names.
            let stop_call = params.stop_on_tool.and_then(|stop_spec| {
                tool_calls
                    .iter()
                    .find(|c| stop_spec.split('|').any(|s| s.trim() == c.tool_name.as_str()))
                    .cloned()
            });

            if let Some(stop) = stop_call {
                let mut assistant_message = Message::assistant(text_buffer.clone());
                if !reasoning_buffer.is_empty() {
                    assistant_message.reasoning_content = Some(reasoning_buffer);
                }
                // Only the handoff call is recorded: sibling tool calls emitted
                // in the same response were never executed, so they must not
                // enter history (strict providers reject an assistant message
                // whose tool_calls lack matching tool responses).
                assistant_message.tool_calls = Some(vec![stop.clone()]);
                history.push(assistant_message);
                // Answer the handoff immediately so the history passed to the
                // next LLM call stays well-formed (assistant tool_calls must be
                // followed by a tool response for the same call_id).
                history.push(Message::tool_result_with_call_id(
                    &stop.call_id,
                    &stop.tool_name,
                    &stop.arguments_json,
                ));
                last_text = text_buffer;
                return Ok(RoleLoopOutcome {
                    history,
                    final_text: last_text,
                    stop_tool_name: Some(stop.tool_name),
                    stop_tool_args: Some(stop.arguments_json),
                    tokens_in: input_tokens,
                    tokens_out: output_tokens,
                    cached_in,
                    tokens_used: input_tokens + output_tokens,
                    exhausted_turns: false,
                });
            }

            let mut tool_calls = tool_calls;
            if tool_calls.is_empty() {
                // Detect an UNCLOSED tool tag (stream cut off mid-call). The
                // old code returned an Err promising "Melakukan retry
                // otomatis..." that NOTHING ever performed — it propagated up
                // and killed the whole phase/run. Self-correct inside this
                // loop instead: ask the model to re-emit the complete call
                // (still bounded by max_turns).
                let truncated_tag = params.allowed_tools.iter().find(|tool_name| {
                    text_buffer.contains(&format!("<{}", tool_name))
                        && !text_buffer.contains(&format!("</{}>", tool_name))
                });
                if let Some(tool_name) = truncated_tag {
                    let mut cut_msg = Message::assistant(text_buffer.clone());
                    if !reasoning_buffer.is_empty() {
                        cut_msg.reasoning_content = Some(reasoning_buffer.clone());
                    }
                    history.push(cut_msg);
                    history.push(Message::user(format!(
                        "[TRUNCATED OUTPUT]: Your previous response was cut off in the middle of \
                         a <{0}> call — no closing </{0}> tag was received, so it was discarded. \
                         Re-emit the COMPLETE <{0}>...</{0}> call now.",
                        tool_name
                    )));
                    continue;
                }
                tool_calls = extract_text_tool_calls(&text_buffer, params.allowed_tools);
            }

            if tool_calls.is_empty() {
                let reasoning_text = reasoning_buffer.clone();
                let mut final_message = Message::assistant(&text_buffer);
                if !reasoning_text.is_empty() {
                    final_message.reasoning_content = Some(reasoning_text);
                }
                history.push(final_message);
                last_text = text_buffer;
                return Ok(RoleLoopOutcome {
                    history,
                    final_text: last_text,
                    stop_tool_name: None,
                    stop_tool_args: None,
                    tokens_in: input_tokens,
                    tokens_out: output_tokens,
                    cached_in,
                    tokens_used: input_tokens + output_tokens,
                    exhausted_turns: false,
                });
            }

            // Model turn with tool calls
            let mut assistant_message = Message::assistant(text_buffer.clone());
            if !reasoning_buffer.is_empty() {
                assistant_message.reasoning_content = Some(reasoning_buffer.clone());
            }
            assistant_message.tool_calls = Some(tool_calls.clone());
            history.push(assistant_message.clone());
            last_text = text_buffer;

            for call in &tool_calls {
                if tool_ctx.cancel.is_cancelled() {
                    emit(AgentEventKind::Error("Run cancelled by user.".to_string()));
                    return Err(AppError::General("Run cancelled by user.".to_string()));
                }
                let result = self
                    .run_tool_call(
                        call,
                        tool_ctx,
                        auto_approve,
                        Some(params.allowed_tools),
                        emit,
                    )
                    .await;

                let truncated_result = crate::agent::tool_registry::ToolResult {
                    success: result.success,
                    output: self.bounded_truncate_middle(
                        &result.output,
                        MAX_TOOL_OUTPUT_TOTAL_CHARS.saturating_sub(tool_output_chars),
                    ),
                    is_error: result.is_error,
                };
                tool_output_chars = tool_output_chars.saturating_add(truncated_result.output.len());
                let tool_message = Message::tool_result_with_call_id(
                    &call.call_id,
                    &call.tool_name,
                    serde_json::to_string(&truncated_result).unwrap_or_else(|_| result.output.clone()),
                );
                history.push(tool_message);
            }
        }

        emit(AgentEventKind::Error(format!(
            "Role '{}' reached maximum turn limit ({}) without calling '{}' — stopping.",
            params.role_name,
            params.max_turns,
            params.stop_on_tool.unwrap_or("<final text>")
        )));
        Ok(RoleLoopOutcome {
            history,
            final_text: last_text,
            stop_tool_name: None,
            stop_tool_args: None,
            tokens_in: input_tokens,
            tokens_out: output_tokens,
            cached_in,
            tokens_used: input_tokens + output_tokens,
            exhausted_turns: true,
        })
    }

    /// Executes a single tool call, transparently splitting malformed
    /// arguments (several concatenated JSON objects — a sloppy-model failure
    /// mode) into separate executions so the intended work actually runs
    /// instead of aborting. Emits per-execution streaming events.
    async fn run_tool_call(
        &self,
        call: &crate::agent::llm_client::ToolCallChunk,
        tool_ctx: &ToolContext,
        auto_approve: bool,
        role_allowed: Option<&[String]>,
        emit: &impl Fn(AgentEventKind),
    ) -> crate::agent::tool_registry::ToolResult {
        use crate::agent::tool_registry::ToolResult;

        // Role gate (run_role_loop only).
        if let Some(allowed) = role_allowed {
            if !allowed.iter().any(|t| t == &call.tool_name) {
                return ToolResult {
                    success: false,
                    output: format!("Tool '{}' is not available for your role.", call.tool_name),
                    is_error: true,
                };
            }
        }

        // Approval gate.
        let requires_approval = self
            .tool_registry
            .get_definitions()
            .iter()
            .find(|d| d.name == call.tool_name)
            .map(|d| d.requires_approval)
            .unwrap_or(false);
        if requires_approval && !auto_approve {
            return ToolResult {
                success: false,
                output: "Tool execution skipped: this tool requires user approval and auto-approve is disabled. Ask the user to enable auto-approve or perform the edit manually.".to_string(),
                is_error: true,
            };
        }

        // Parse arguments; if malformed, attempt to split concatenated JSON
        // objects into the individual tool calls the model intended.
        let params_list: Vec<serde_json::Value> = match parse_tool_args(call) {
            Some(v) => vec![v],
            None => match split_concatenated_json(&call.arguments_json) {
                Some(parts) => parts,
                None => {
                    return ToolResult {
                        success: false,
                        output: format!(
                            "Tool call aborted: arguments were not valid JSON (parse error) and could \
                             not be split into separate calls. Re-issue the tool call with a single \
                             valid JSON object matching the tool's schema. Arguments received: {}",
                            truncate_output(&call.arguments_json, 500)
                        ),
                        is_error: true,
                    };
                }
            },
        };

        let split = params_list.len() > 1;
        let mut outputs: Vec<String> = Vec::new();
        let mut all_ok = true;
        for (i, params) in params_list.iter().enumerate() {
            if tool_ctx.cancel.is_cancelled() {
                break;
            }
            let label = if split {
                format!("{} (split {}/{})", call.tool_name, i + 1, params_list.len())
            } else {
                call.tool_name.clone()
            };
            emit(AgentEventKind::ToolCallStarted {
                tool_name: label.clone(),
                call_id: call.call_id.clone(),
                arguments_json: serde_json::to_string(params).unwrap_or_default(),
            });

            let result = match self
                .tool_registry
                .execute_tool(&call.tool_name, params.clone(), tool_ctx)
                .await
            {
                Ok(res) => res,
                Err(e) => ToolResult {
                    success: false,
                    output: format!("Tool execution error: {}", e),
                    is_error: true,
                },
            };
            all_ok = all_ok && !result.is_error;

            emit(AgentEventKind::ToolCallCompleted {
                tool_name: label,
                call_id: call.call_id.clone(),
                output: truncate_output(&result.output, 4000),
            });

            if split {
                outputs.push(format!(
                    "[{} — split call {}/{}]\n{}",
                    call.tool_name,
                    i + 1,
                    params_list.len(),
                    result.output
                ));
            } else {
                outputs.push(result.output);
            }
        }

        ToolResult {
            success: all_ok,
            output: outputs.join("\n"),
            is_error: !all_ok,
        }
    }
}

/// Builds the "final turn" nudge message injected into the last LLM request of a
/// role loop. It tells the model to conclude with its handoff tool (or a plain
/// text summary for roles without one) instead of spending the turn on tools.
fn last_turn_nudge(stop_on_tool: Option<&str>) -> String {
    match stop_on_tool {
        Some(name) => format!(
            "[SYSTEM] This is your final turn. Call `{}` now with your best final result. Do not call any other tools.",
            name
        ),
        None => "[SYSTEM] This is your final turn. Finish your remaining work now and write a brief summary of what you did. Do not call any more tools.".to_string(),
    }
}

fn truncate_output(output: &str, max: usize) -> String {
    if output.chars().count() <= max {
        output.to_string()
    } else {
        format!(
            "{}… (truncated)",
            output.chars().take(max).collect::<String>()
        )
    }
}

/// Tools whose arguments DIRECTLY mutate user-visible state. For these a
/// "repaired" truncated JSON is not a convenience but a hazard: closing a
/// stream-cut string mid-content means writing HALF A FILE or running HALF A
/// COMMAND as if it were complete. Truncation repair stays available for
/// read-only tools where a best-effort parse is harmless.
const MUTATING_TOOLS: &[&str] = &["write_file", "multi_replace_file", "run_command"];

/// Parses a tool call's `arguments_json`. Returns `None` when the JSON is
/// malformed (e.g. concatenated objects from a sloppy model) so the caller can
/// split it or report a precise, self-correcting error instead of silently
/// running the tool with empty params.
fn parse_tool_args(
    call: &crate::agent::llm_client::ToolCallChunk,
) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&call.arguments_json) {
        return Some(v);
    }
    // Never guess-and-close truncated JSON for mutating tools: the repair
    // would execute partial content as if it were complete.
    if MUTATING_TOOLS.iter().any(|t| *t == call.tool_name) {
        return None;
    }
    // Attempt resilient repair for truncated/unclosed JSON strings
    repair_truncated_json(&call.arguments_json)
        .or_else(|| parse_xml_tool_args(&call.arguments_json))
}

fn repair_truncated_json(raw: &str) -> Option<serde_json::Value> {
    let trimmed = raw.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let candidates = [
        format!("{}\"}}", trimmed),
        format!("{}}}", trimmed),
        format!("{}\"}}}}", trimmed),
        format!("{}}}}}", trimmed),
    ];
    for cand in &candidates {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(cand) {
            return Some(v);
        }
    }
    None
}

fn parse_xml_tool_args(raw: &str) -> Option<serde_json::Value> {
    let mut map = serde_json::Map::new();
    let trimmed = raw.trim();
    
    // Generic regex to capture any <tag>value</tag> or <tag attr="val">value</tag>
    if let Ok(re) = regex::Regex::new(r#"<([a-zA-Z0-9_-]+)(?:\s+[^>]*)?>([\s\S]*?)</\1>"#) {
        for cap in re.captures_iter(trimmed) {
            if let (Some(k), Some(v)) = (cap.get(1), cap.get(2)) {
                let key = k.as_str();
                let mut val = v.as_str().to_string();
                if val.starts_with("<![CDATA[") && val.ends_with("]]>") {
                    val = val[9..val.len() - 3].to_string();
                }
                if let Ok(json_v) = serde_json::from_str::<serde_json::Value>(&val) {
                    if json_v.is_array() || json_v.is_object() || json_v.is_boolean() || json_v.is_number() {
                        map.insert(key.to_string(), json_v);
                        continue;
                    }
                }
                map.insert(key.to_string(), serde_json::Value::String(val));
            }
        }
    }
    
    if !map.is_empty() {
        Some(serde_json::Value::Object(map))
    } else {
        None
    }
}

pub fn extract_text_tool_calls(text: &str, allowed: &[String]) -> Vec<crate::agent::llm_client::ToolCallChunk> {
    let mut out = Vec::new();
    for tool_name in allowed {
        let open_tag = format!("<{}", tool_name);
        let mut search_from = 0;

        while let Some(rel_pos) = text[search_from..].find(&open_tag) {
            let pos = search_from + rel_pos;
            let after_open = &text[pos + open_tag.len()..];

            // Check if it's an exact tag match (e.g. `<write_file>` or `<write_file `)
            if !after_open.starts_with('>') && !after_open.starts_with(' ') && !after_open.starts_with('\n') && !after_open.starts_with('\t') {
                search_from = pos + open_tag.len();
                continue;
            }

            let close_tag = format!("</{}>", tool_name);
            let Some(end_rel) = after_open.find(&close_tag) else {
                // If closing tag is missing, the tool call is incomplete/truncated.
                // Do NOT parse or execute it.
                search_from = pos + open_tag.len();
                continue;
            };

            let inside = &after_open[..end_rel];
            search_from = pos + open_tag.len() + end_rel + close_tag.len();

            let mut params = serde_json::Map::new();

            if let Some(tag_header_end) = inside.find('>') {
                let header = &inside[..tag_header_end];
                let body = &inside[tag_header_end + 1..];

                // 1. Extract attributes from opening tag if any (e.g. `<write_file path="foo">`)
                if let Ok(re) = regex::Regex::new(r#"([a-zA-Z0-9_-]+)=["']([^"']*)["']"#) {
                    for cap in re.captures_iter(header) {
                        if let (Some(k), Some(v)) = (cap.get(1), cap.get(2)) {
                            params.insert(k.as_str().to_string(), serde_json::Value::String(v.as_str().to_string()));
                        }
                    }
                }

                // 2. Extract child tags (e.g. `<path>...</path>`, `<content>...</content>`)
                if let Ok(child_re) = regex::Regex::new(r#"<([a-zA-Z0-9_-]+)>([\s\S]*?)</\1>"#) {
                    for cap in child_re.captures_iter(body) {
                        if let (Some(k), Some(v)) = (cap.get(1), cap.get(2)) {
                            let key = k.as_str();
                            let mut val = v.as_str().to_string();
                            if val.starts_with("<![CDATA[") && val.ends_with("]]>") {
                                val = val[9..val.len() - 3].to_string();
                            }
                            if let Ok(json_v) = serde_json::from_str::<serde_json::Value>(&val) {
                                if json_v.is_array() || json_v.is_object() || json_v.is_boolean() || json_v.is_number() {
                                    params.insert(key.to_string(), json_v);
                                    continue;
                                }
                            }
                            params.insert(key.to_string(), serde_json::Value::String(val));
                        }
                    }
                }
            }

            if !params.is_empty() {
                out.push(crate::agent::llm_client::ToolCallChunk {
                    call_id: format!("call_{}", uuid::Uuid::new_v4().simple()),
                    tool_name: tool_name.clone(),
                    arguments_json: serde_json::Value::Object(params).to_string(),
                });
            }
        }
    }
    out
}

/// Attempts to split a malformed arguments string (several complete JSON
/// objects concatenated back-to-back, a known sloppy-model failure mode) into
/// the individual objects. Returns `None` unless EVERY piece parses as a
/// complete JSON object and there are at least two.
fn split_concatenated_json(args: &str) -> Option<Vec<serde_json::Value>> {
    if serde_json::from_str::<serde_json::Value>(args).is_ok() {
        return None; // Not malformed after all.
    }
    let bytes = args.as_bytes();
    let len = bytes.len();
    let mut values: Vec<serde_json::Value> = Vec::new();
    let mut pos = 0usize;

    while pos < len {
        while pos < len && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= len {
            break;
        }
        if bytes[pos] != b'{' {
            return None; // Only top-level objects can be split.
        }
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escaped = false;
        let mut end = len;
        let mut found = false;
        for i in pos..len {
            let c = bytes[i];
            if in_string {
                if escaped {
                    escaped = false;
                } else if c == b'\\' {
                    escaped = true;
                } else if c == b'"' {
                    in_string = false;
                }
            } else {
                match c {
                    b'"' => in_string = true,
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = i + 1;
                            found = true;
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        if !found {
            return None; // Unbalanced braces.
        }
        let piece = &args[pos..end];
        match serde_json::from_str::<serde_json::Value>(piece) {
            Ok(v) => values.push(v),
            Err(_) => return None,
        }
        pos = end;
    }

    if values.len() >= 2 {
        Some(values)
    } else {
        None
    }
}

pub fn truncate_middle(output: &str, max_chars: usize) -> String {
    let char_count = output.chars().count();
    if char_count <= max_chars {
        output.to_string()
    } else {
        let half = max_chars / 2;
        let head: String = output.chars().take(half).collect();
        let tail: String = output.chars().skip(char_count - half).collect();
        let truncated_count = char_count - max_chars;
        format!(
            "{}\n\n... [TRUNCATED {} CHARACTERS TO SAVE CONTEXT WINDOW. USE rlm_python OR LINE-RANGE READS TO INSPECT SPECIFIC SECTIONS] ...\n\n{}",
            head, truncated_count, tail
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_last_turn_nudge_mentions_handoff_tool() {
        let nudge = last_turn_nudge(Some("submit_plan"));
        assert!(nudge.contains("submit_plan"));
        assert!(nudge.contains("final turn"));
    }

    #[test]
    fn test_is_transient_llm_error_classifies_status_and_hints() {
        let gen = |m: &str| AppError::General(m.to_string());

        // Rate limit / server errors / connection problems are retryable.
        assert!(is_transient_llm_error(&gen("OpenAI API error (429 Too Many Requests): No deployments available for selected model, Try again in 5 seconds. cooldown_list=['f0264a4f-2287-4eb6-8139-5bfc67e9d210']")));
        assert!(is_transient_llm_error(&gen("OpenAI API error (502 Bad Gateway): upstream unavailable")));
        assert!(is_transient_llm_error(&gen("OpenAI API error (503 Service Unavailable)")));
        assert!(is_transient_llm_error(&gen("OpenAI API error (500): internal")));
        assert!(is_transient_llm_error(&gen("OpenAI SSE error: connection reset")));
        assert!(is_transient_llm_error(&gen("OpenAI request failed: request timed out")));
        assert!(is_transient_llm_error(&AppError::RateLimitExceeded("gateway cap".into())));

        // Auth / schema / 400 errors are permanent and must NOT be retried.
        assert!(!is_transient_llm_error(&gen("OpenAI API error (401 Unauthorized): invalid api key")));
        assert!(!is_transient_llm_error(&gen("OpenAI API error (400 Bad Request): messages schema mismatch")));
        assert!(!is_transient_llm_error(&gen("OpenAI API error (404): model not found")));

        // Upstream status-coded errors (new Api variant) ARE retried: 401 gets a
        // forced hub-session refresh before retry, 408/409/429/5xx are transient.
        assert!(is_transient_llm_error(&AppError::Api { status: 401, message: "Unauthorized or invalid developer token".into() }));
        assert!(is_transient_llm_error(&AppError::Api { status: 408, message: "timeout".into() }));
        assert!(is_transient_llm_error(&AppError::Api { status: 429, message: "slow down".into() }));
        assert!(is_transient_llm_error(&AppError::Api { status: 500, message: "boom".into() }));
        assert!(is_transient_llm_error(&AppError::Api { status: 503, message: "busy".into() }));
        assert!(!is_transient_llm_error(&AppError::Api { status: 400, message: "bad schema".into() }));
        assert!(!is_transient_llm_error(&gen("OpenAI API error (413): payload too large")));
    }

    #[test]
    fn test_parse_retry_after_seconds_extracts_hint() {
        let gen = |m: &str| AppError::General(m.to_string());

        assert_eq!(
            parse_retry_after_seconds(&gen("No deployments available for selected model, Try again in 5 seconds.")),
            Some(5)
        );
        assert_eq!(
            parse_retry_after_seconds(&gen("Retry-After: 30")),
            Some(30)
        );
        // No hint / absurd values fall back to None so the caller uses its own backoff.
        assert_eq!(parse_retry_after_seconds(&gen("OpenAI API error (503)")), None);
        assert_eq!(parse_retry_after_seconds(&gen("Try again in 9999 seconds")), None);
    }

    #[test]
    fn test_last_turn_nudge_without_handoff_asks_for_summary() {
        let nudge = last_turn_nudge(None);
        assert!(!nudge.contains("submit_"));
        assert!(nudge.contains("final turn"));
        assert!(nudge.contains("summary"));
    }

    #[test]
    fn test_parse_tool_args_rejects_malformed_json() {
        use crate::agent::llm_client::ToolCallChunk;

        // Valid single JSON object parses.
        let ok_call = ToolCallChunk {
            call_id: "c1".into(),
            tool_name: "grep_search".into(),
            arguments_json: r#"{"pattern": "pub fn", "path": "src"}"#.into(),
        };
        assert!(parse_tool_args(&ok_call).is_some());

        // Concatenated objects (the observed failure mode) must be rejected so
        // the caller can split them instead of silently running with empty params.
        let malformed_call = ToolCallChunk {
            call_id: "c2".into(),
            tool_name: "batch_file_read".into(),
            arguments_json: r#"{"paths": ["a.rs"]}{"pattern": "x"}"#.into(),
        };
        assert!(parse_tool_args(&malformed_call).is_none());

        let empty_call = ToolCallChunk {
            call_id: "c3".into(),
            tool_name: "grep_search".into(),
            arguments_json: String::new(),
        };
        assert!(parse_tool_args(&empty_call).is_none());
    }

    #[test]
    fn test_split_concatenated_json() {
        // Two complete objects → split into two.
        let args = r#"{"pattern": "pub fn", "path": "a"}{"pattern": "struct", "path": "b"}"#;
        let parts = split_concatenated_json(args).expect("two objects must split");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["pattern"], "pub fn");
        assert_eq!(parts[1]["path"], "b");

        // A single valid object is NOT malformed → None.
        assert!(split_concatenated_json(r#"{"pattern": "x"}"#).is_none());

        // Trailing garbage → None (must not silently drop it).
        assert!(split_concatenated_json(r#"{"a": 1} garbage"#).is_none());

        // Objects containing nested braces and braces inside strings split
        // correctly.
        let nested = r#"{"pattern": "fn a() { }", "x": "}"}{"b": 2}"#;
        let parts = split_concatenated_json(nested).expect("nested objects must split");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["pattern"], "fn a() { }");
        assert_eq!(parts[1]["b"], 2);

        // Unbalanced braces → None.
        assert!(split_concatenated_json(r#"{"a": 1"#).is_none());
    }
}
