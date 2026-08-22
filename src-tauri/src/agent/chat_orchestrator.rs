use std::sync::Arc;
use serde::Deserialize;
use tauri::ipc::Channel;

use crate::agent::llm_client::Message;
use crate::agent::orchestrator::{AgentEvent, AgentEventKind, AgentOrchestrator, RoleLoopParams};
use crate::agent::prompt_composer::PromptComposer;
use crate::agent::roles::{resolve_role_provider, AgentRole};
use crate::agent::swarm::{
    render_plan_markdown, SwarmOrchestrator, SwarmOutcome, SwarmPlan, SwarmPlanTask,
    TranscriptCollector, TranscriptCollectorRef, TurnLedger, Verdict,
};
use crate::agent::tool_registry::{ToolContext, ToolRegistry};
use crate::error::{AppError, Result};

const MAX_COORDINATOR_TURNS: usize = 16;
const STOP_DELEGATION_TOOLS: &str =
    "call_rlm_research|call_thinker_direction|call_planning_swarm|call_executor";

#[derive(Debug, Deserialize)]
struct CallRlmResearchArgs {
    query: String,
    #[serde(default)]
    target_files: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CallThinkerDirectionArgs {
    goal: String,
    #[serde(default)]
    research_brief: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CallPlanningSwarmArgs {
    direction: String,
    user_goal: String,
}

#[derive(Debug, Deserialize)]
struct CallExecutorArgs {
    task_kind: String,
    task_description: String,
    #[serde(default)]
    target_files: Vec<String>,
    #[serde(default)]
    plan_file: Option<String>,
}

/// Orchestrates the conversational Chat Coordinator frontline agent.
pub struct ChatOrchestrator {
    inner: AgentOrchestrator,
    swarm: SwarmOrchestrator,
    tool_registry: Arc<ToolRegistry>,
}

impl ChatOrchestrator {
    pub fn new(tool_registry: Arc<ToolRegistry>) -> Self {
        Self {
            inner: AgentOrchestrator::new(tool_registry.clone()),
            swarm: SwarmOrchestrator::new(tool_registry.clone()),
            tool_registry,
        }
    }

    /// Runs a multi-turn chat coordinator conversation turn.
    pub async fn run_coordinator_chat(
        &self,
        messages: &[Message],
        tool_ctx: &ToolContext,
        auto_approve: bool,
        on_event: &Channel<AgentEvent>,
        transcript: &TranscriptCollectorRef,
    ) -> Result<SwarmOutcome> {
        let project_root = tool_ctx.project_root.clone();
        let app_data_dir = tool_ctx.app_data_dir.clone();
        let transcript_cell = transcript.clone();
        let run_id = tool_ctx.session_id.clone().unwrap_or_default();

        let cancel_flag = tool_ctx.cancel.clone();
        let emit = move |kind: AgentEventKind| {
            if let Ok(mut col) = transcript_cell.lock() {
                let col: &mut TranscriptCollector = &mut *col;
                col.record(&kind);
            }
            if on_event.send(AgentEvent { kind }).is_err() {
                // The frontend event channel is gone (webview reload/crash):
                // nobody can see progress or resolve approval gates anymore.
                // Cancel the run instead of burning tokens invisibly until
                // completion.
                cancel_flag.cancel();
            }
        };

        let coordinator_provider =
            resolve_role_provider(AgentRole::ChatCoordinator, &app_data_dir).await?;
        let spec = AgentRole::ChatCoordinator.spec();

        emit(AgentEventKind::PhaseStarted {
            role: AgentRole::ChatCoordinator.key().to_string(),
            label: "Chat Coordinator: analyzing request".to_string(),
            model: coordinator_provider.name().to_string(),
        });

        let mut conversation: Vec<Message> = messages.to_vec();
        let mut total_tokens = 0usize;
        let mut total_tokens_in = 0usize;
        let mut total_tokens_out = 0usize;
        let mut total_cached_in = 0usize;

        let mut final_answer_text = String::new();
        let mut produced_plan: Option<SwarmPlan> = None;
        let mut produced_verdict: Option<Verdict> = None;
        let mut ledger = TurnLedger {
            brief_digest: None,
            plan_markdown: None,
            plan_status: None,
            execution_review: None,
            final_answer: String::new(),
        };
        let mut shared: Vec<Message> = messages.to_vec();
        let mut thinker_history: Vec<Message> = Vec::new();
        let mut exec_notes: Vec<String> = Vec::new();

        let max_turns = spec.max_turns.min(MAX_COORDINATOR_TURNS);

        for _turn in 0..max_turns {
            if tool_ctx.cancel.is_cancelled() {
                emit(AgentEventKind::Error("Chat cancelled by user.".to_string()));
                return Err(AppError::General("Chat cancelled by user.".to_string()));
            }

            let outcome = self
                .inner
                .run_role_loop(
                    RoleLoopParams {
                        system_prompt: PromptComposer::compose_role_prompt(
                            AgentRole::ChatCoordinator,
                            &project_root,
                        ),
                        messages: conversation.clone(),
                        allowed_tools: &spec.allowed_tools,
                        provider: coordinator_provider.clone(),
                        max_turns: 2,
                        temperature: spec.temperature,
                        stop_on_tool: Some(STOP_DELEGATION_TOOLS),
                        role_name: AgentRole::ChatCoordinator.key(),
                    },
                    tool_ctx,
                    auto_approve,
                    &emit,
                )
                .await?;

            total_tokens += outcome.tokens_used;
            total_tokens_in += outcome.tokens_in;
            total_tokens_out += outcome.tokens_out;
            total_cached_in += outcome.cached_in;

            conversation = outcome.history;

            // Check if Coordinator invoked a delegation tool
            if let Some(tool_name) = outcome.stop_tool_name {
                let args_json = outcome.stop_tool_args.unwrap_or_default();

                let tool_result_str = match tool_name.as_str() {
                    "call_rlm_research" => {
                        let args: CallRlmResearchArgs = serde_json::from_str(&args_json)
                            .map_err(|e| AppError::General(format!("Invalid call_rlm_research args: {}", e)))?;
                        let rlm_req = vec![Message::user(format!(
                            "Research query: {}\nTarget files: {:?}",
                            args.query, args.target_files
                        ))];
                        let brief = self
                            .swarm
                            .run_rlm_phase(
                                &rlm_req,
                                tool_ctx,
                                auto_approve,
                                &emit,
                                &mut total_tokens,
                                &mut total_tokens_in,
                                &mut total_tokens_out,
                                &mut total_cached_in,
                                &mut ledger,
                                &mut shared,
                                &run_id,
                            )
                            .await?;
                        format!("[RLM RESEARCH BRIEF]\n{}", brief)
                    }
                    "call_thinker_direction" => {
                        let args: CallThinkerDirectionArgs = serde_json::from_str(&args_json)
                            .map_err(|e| AppError::General(format!("Invalid call_thinker_direction args: {}", e)))?;
                        shared.push(Message::user(format!(
                            "[DIRECTION GOAL]: {}\nResearch: {}",
                            args.goal,
                            args.research_brief.as_deref().unwrap_or("(none)")
                        )));
                        let dir_opt = self
                            .swarm
                            .run_thinker_direction_phase(
                                tool_ctx,
                                auto_approve,
                                &emit,
                                false,
                                &mut total_tokens,
                                &mut total_tokens_in,
                                &mut total_tokens_out,
                                &mut total_cached_in,
                                &mut ledger,
                                &mut shared,
                                &mut thinker_history,
                                &run_id,
                            )
                            .await?;
                        format!(
                            "[THINKER DIRECTION]\n{}",
                            dir_opt.unwrap_or_else(|| "NO_FILE_CHANGES".into())
                        )
                    }
                    "call_planning_swarm" => {
                        let args: CallPlanningSwarmArgs = serde_json::from_str(&args_json)
                            .map_err(|e| AppError::General(format!("Invalid call_planning_swarm args: {}", e)))?;
                        shared.push(Message::user(format!(
                            "[PLANNING GOAL]: {}\nDirection: {}",
                            args.user_goal, args.direction
                        )));
                        let plan = self
                            .swarm
                            .run_planning_phase(
                                &thinker_history,
                                tool_ctx,
                                auto_approve,
                                &emit,
                                true,
                                &mut total_tokens,
                                &mut total_tokens_in,
                                &mut total_tokens_out,
                                &mut total_cached_in,
                                &mut ledger,
                                &mut shared,
                                &run_id,
                            )
                            .await?;
                        let plan_md = render_plan_markdown(&plan);
                        produced_plan = Some(plan);
                        format!("[PLANNING SWARM RESULT]\n{}", plan_md)
                    }
                    "call_executor" => {
                        let args: CallExecutorArgs = serde_json::from_str(&args_json)
                            .map_err(|e| AppError::General(format!("Invalid call_executor args: {}", e)))?;
                        let plan = if let Some(ref p) = produced_plan {
                            p.clone()
                        } else {
                            SwarmPlan {
                                goal: args.task_description.clone(),
                                architecture: None,
                                tasks: vec![SwarmPlanTask {
                                    id: 1,
                                    kind: args.task_kind.clone(),
                                    description: args.task_description.clone(),
                                    context: args.plan_file.clone(),
                                    files: args.target_files.clone(),
                                    acceptance: format!("Execute {}", args.task_description),
                                }],
                                risks: None,
                            }
                        };
                        let (verdict_opt, verdict_state) = self
                            .swarm
                            .run_executor_phase(
                                &plan,
                                None,
                                false,
                                tool_ctx,
                                auto_approve,
                                &emit,
                                &mut total_tokens,
                                &mut total_tokens_in,
                                &mut total_tokens_out,
                                &mut total_cached_in,
                                &mut ledger,
                                &mut shared,
                                &mut exec_notes,
                                &run_id,
                            )
                            .await?;
                        if let Some(v) = verdict_opt {
                            produced_verdict = Some(v);
                        }
                        format!(
                            "[EXECUTOR RESULT]\nState: {}\nNotes:\n{}",
                            verdict_state,
                            exec_notes.join("\n")
                        )
                    }
                    unknown => {
                        format!("Error: unknown delegation tool `{}`", unknown)
                    }
                };

                // Replace the placeholder tool-result (the last message in conversation) with the real sub-agent output
                if let Some(last_msg) = conversation.last_mut() {
                    if last_msg.role == crate::agent::llm_client::MessageRole::Tool {
                        last_msg.content = tool_result_str;
                    }
                }

                // Continue loop so Coordinator synthesizes the sub-agent output
                continue;
            }

            // No delegation tool called -> direct final answer!
            final_answer_text = outcome.final_text;
            emit(AgentEventKind::PhaseCompleted {
                role: AgentRole::ChatCoordinator.key().to_string(),
                summary: "Response completed".to_string(),
                tokens_in: outcome.tokens_in,
                tokens_out: outcome.tokens_out,
                cached_in: outcome.cached_in,
            });
            break;
        }

        if final_answer_text.is_empty() {
            final_answer_text = if !exec_notes.is_empty() {
                format!("Execution completed:\n{}", exec_notes.join("\n"))
            } else if let Some(ref plan) = produced_plan {
                format!("Planning completed:\n{}", render_plan_markdown(plan))
            } else {
                "Coordinator conversation completed.".to_string()
            };
            emit(AgentEventKind::PhaseCompleted {
                role: AgentRole::ChatCoordinator.key().to_string(),
                summary: "Response completed".to_string(),
                tokens_in: 0,
                tokens_out: 0,
                cached_in: 0,
            });
        }

        emit(AgentEventKind::Finished {
            total_tokens_used: total_tokens,
            tokens_in: total_tokens_in,
            tokens_out: total_tokens_out,
            cached_in: total_cached_in,
        });

        ledger.final_answer = final_answer_text.clone();

        let transcript_data = transcript
            .lock()
            .map(|mut c: std::sync::MutexGuard<TranscriptCollector>| c.finish())
            .unwrap_or_default();

        Ok(SwarmOutcome {
            final_answer: final_answer_text,
            plan: produced_plan,
            verdict: produced_verdict,
            tokens_used: total_tokens,
            ledger,
            transcript: transcript_data,
        })
    }
}
