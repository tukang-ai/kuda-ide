use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tauri::ipc::Channel;
use crate::error::{AppError, Result};
use crate::agent::chat_history::{PhaseRecord, PhaseToolCall};
use crate::agent::llm_client::{LlmProvider, Message, MessageRole};
use crate::agent::orchestrator::{
    AgentEvent, AgentEventKind, AgentOrchestrator, RoleLoopOutcome, RoleLoopParams,
};
use crate::agent::prompt_composer::PromptComposer;
use crate::agent::provider_config::ProviderConfigManager;
use crate::agent::rlm_cache::{self, CacheDecision, ManifestDiff, ProjectCache};
use crate::agent::rlm_kernel::get_rlm_manager;
use crate::agent::roles::{resolve_role_provider, AgentRole};
use crate::agent::tool_registry::ToolContext;
use crate::diff_engine::calculator::{ChangeKind, DiffCalculator};
use crate::file_system::io::FileSystemIO;
use crate::security::PathGuard;

/// One fix-round maximum so costs stay bounded.
const MAX_FIX_ROUNDS: usize = 1;
/// Max RLM (Model + Verifier) rounds: Model collects, Verifier audits; if gaps
/// are found the Model gets one more collection round.
const MAX_RLM_ROUNDS: usize = 2;
/// Turn budget for the single "re-submit your audit" retry after the RLM
/// Verifier produces an unparseable audit document. Enough for one more
/// submit_audit call.
const RLM_VERIFIER_RETRY_TURNS: usize = 2;
/// Max times the Thinker may hand off to the RLM researcher for additional
/// data during one run (prevents research ping-pong and shared-context bloat).
const MAX_THINKER_RESEARCH_REQUESTS: usize = 2;
/// Turn budget for one RLM supplement round triggered by the Thinker.
const RLM_SUPPLEMENT_MAX_TURNS: usize = 24;
/// Max changed lines of diff reported back into the shared context per file.
const MAX_DIFF_LINES_PER_FILE: usize = 120;
/// Max total chars of one executor report inside the shared context.
const MAX_REPORT_CHARS: usize = 6000;
/// A cached research older than this many days is treated as fresh research
/// even when the tree is byte-identical (external factors may have changed).
const MAX_BRIEF_AGE_DAYS: i64 = 30;
/// If more than this fraction of the tracked tree changed, the cached brief is
/// too stale to anchor an incremental refresh.
const CHANGED_RATIO_THRESHOLD: f32 = 0.30;
/// Turn budget for the RLM Model during a sufficiency check: it must verify
/// the cached brief against the current tree, capture snippets, WRITE the full
/// brief to `.kuda/brief.md`, then call `submit_brief` — so it shares the same
/// generous budget as a fresh research round.
const SUFFICIENCY_MAX_TURNS: usize = 24;

// ── Ledger budgets (token economy) ─────────────────────────────────────────
/// Max chars of the `[RESEARCH BRIEF]` segment of one turn's ledger block.
const LEDGER_BRIEF_CHARS: usize = 1500;
/// Max chars of the `[PLAN]` segment.
const LEDGER_PLAN_CHARS: usize = 2000;
/// Max chars of the `[EXECUTION REVIEW]` segment.
const LEDGER_EXEC_CHARS: usize = 2000;
/// Max chars of the `[FINAL ANSWER]` segment.
const LEDGER_ANSWER_CHARS: usize = 1500;
/// Max chars of one tool-call output stored in the display transcript.
const PHASE_TOOL_OUTPUT_CHARS: usize = 600;
/// Max gate rounds (plan revisions/reviews) per turn before the gate only
/// offers execute/cancel.
const MAX_PLAN_GATE_ROUNDS: usize = 8;
/// Safety backstop for the Planning Writer draft loop (Thinker review). The
/// old hard cap of 2 was REMOVED: each round = one Planning Writer draft +
/// one Thinker review, and the loop now runs until the Thinker APPROVES or a
/// no-progress guard fires (identical revision notes, or a rewrite that left
/// the plan unchanged). This backstop only guards against a pathological
/// reviewer inventing new revisions forever — normal runs end far earlier.
const PLAN_WRITER_SAFETY_CAP: usize = 10;
/// Safety backstop for the Reviewer-utama improvement loop. The old hard cap
/// of 2 was REMOVED: each round = Reviewer utama audit (Kimi-K3, read-only) +
/// Thinker evaluation + Planning Writer rewrite, and the loop now runs until
/// the Reviewer APPROVES, so the final plan is always re-audited after a
/// rewrite (a plan rejected on the last round is no longer shipped un-audited).
/// No-progress guards (identical directions, unchanged plan, failed rewrite)
/// break out early; this backstop only guards against a pathological reviewer
/// inventing new directions forever.
const PLAN_IMPROVE_SAFETY_CAP: usize = 10;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SwarmPlanTask {
    #[serde(default)]
    pub id: u32,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub description: String,
    /// WHY this task exists and everything the (literal) executor must know:
    /// design rationale, background from the brief, what NOT to touch.
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub acceptance: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SwarmPlan {
    #[serde(default)]
    pub goal: String,
    /// System design / architecture discussion: components & modules, data
    /// flow, runtime & concurrency, error handling, storage, integrations.
    /// MANDATORY for any non-trivial plan — tasks implement THIS design.
    #[serde(default)]
    pub architecture: Option<String>,
    /// Assumptions / unknowns the executors must verify before or during work.
    #[serde(default)]
    pub risks: Option<String>,
    #[serde(default)]
    pub tasks: Vec<SwarmPlanTask>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VerdictIssue {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub kind: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Verdict {
    #[serde(default)]
    pub passed: bool,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub issues: Vec<VerdictIssue>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AuditGap {
    #[serde(default)]
    pub what: String,
    #[serde(default, rename = "where")]
    pub where_path: String,
    #[serde(default)]
    pub why_needed: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ContextAudit {
    #[serde(default)]
    pub complete: bool,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub missing: Vec<AuditGap>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct BriefFile {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub why: String,
    #[serde(default)]
    pub key_symbols: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct BriefSnippet {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub lines: String,
    #[serde(default)]
    pub content: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct BriefExternal {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub why: String,
    #[serde(default)]
    pub verified_safe: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ResearchBrief {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub key_files: Vec<BriefFile>,
    #[serde(default)]
    pub relevant_snippets: Vec<BriefSnippet>,
    #[serde(default)]
    pub conventions: String,
    #[serde(default)]
    pub risks_unknowns: Vec<String>,
    #[serde(default)]
    pub external_pulls: Vec<BriefExternal>,
}

/// One turn's condensed "ledger" block: everything the NEXT turn needs to know
/// about this turn's thinking, without the executor transcript. Rendered into a
/// single assistant `Message` (name = "ledger") and appended to the session.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TurnLedger {
    /// Validated RLM brief digest produced this turn.
    pub brief_digest: Option<String>,
    /// Final plan that was executed / approved (markdown).
    pub plan_markdown: Option<String>,
    /// Approval status: "approved (review×m, revision×n)", "auto (gate off)",
    /// "CANCELLED AT GATE", or None for direct-answer / failure paths.
    pub plan_status: Option<String>,
    /// Condensed per-file diff + verdict + top issues.
    pub execution_review: Option<String>,
    /// Final Thinker answer.
    pub final_answer: String,
}

/// Where a failed run can resume from. The checkpoint is refreshed at every
/// phase boundary (and after every completed executor task) so a failure can
/// continue from exactly where it stopped.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ResumePhase {
    /// RLM research (Model + Verifier) is done and the brief is validated.
    /// Resume at the Thinker direction checkpoint.
    Direction,
    /// RLM + direction are done and the direction was approved. Resume directly
    /// at the Thinker full-plan stage (skips re-research AND the direction gate).
    Planning,
    /// The plan was approved and execution started. Resume at the next pending
    /// executor task with the approved plan + completed-task context intact.
    Executing,
}

/// Snapshot of a run's shared context so a failed run can continue from the
/// phase boundary it last crossed instead of restarting from the user prompt.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RunCheckpoint {
    pub phase: ResumePhase,
    /// The shared swarm context (brief digest, direction, plan) accumulated so far.
    pub shared: Vec<Message>,
    pub total_tokens: usize,
    /// INPUT tokens consumed so far, kept separately so a resume does not
    /// double-count the outputs as inputs (total_tokens = in + out).
    #[serde(default)]
    pub tokens_in: usize,
    /// OUTPUT tokens consumed so far. Kept in the checkpoint so a resume does
    /// not reset the run-wide out/cached totals to zero (the `Finished` event
    /// and the `tokens_used == tokens_in + tokens_out` invariant must survive
    /// a resume).
    #[serde(default)]
    pub tokens_out: usize,
    /// Cached-input tokens consumed so far.
    #[serde(default)]
    pub cached_in: usize,
    pub ledger: TurnLedger,
    /// The approved plan (required for the `Executing` phase).
    #[serde(default)]
    pub final_plan: Option<SwarmPlan>,
    /// Executor tasks that are NOT yet completed (resume runs only these).
    #[serde(default)]
    pub pending_tasks: Vec<SwarmPlanTask>,
    /// The current round's executor edit transcripts (for the Executor Reviewer).
    #[serde(default)]
    pub executor_logs: Vec<Message>,
    /// Number of fix rounds already used.
    #[serde(default)]
    pub fix_round: usize,
    /// Condensed `[EXEC]` one-liners produced so far (for the ledger).
    #[serde(default)]
    pub exec_notes: Vec<String>,
}

/// File path for a run checkpoint, rejecting any `run_id` that is not a plain
/// single-component id. This is the path-traversal guard for checkpoint I/O:
/// a hostile `run_id` like `../hub_credentials` must never escape
/// `resume_runs` (it could otherwise overwrite arbitrary app-data files).
fn checkpoint_path(app_data_dir: &Path, run_id: &str) -> Option<std::path::PathBuf> {
    if !crate::agent::chat_history::is_safe_id(run_id) {
        tracing::warn!("Rejected checkpoint path for unsafe run_id: {:?}", run_id);
        return None;
    }
    Some(app_data_dir.join("resume_runs").join(format!("{}.json", run_id)))
}

fn write_checkpoint(app_data_dir: &Path, run_id: &str, cp: &RunCheckpoint) {
    let Some(path) = checkpoint_path(app_data_dir, run_id) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(cp) {
        // Atomic write (tmp + rename): a crash mid-write must never corrupt the
        // resume file. `load_checkpoint` swallows parse errors, so a torn file
        // used to make `agent_resume_run` silently report "no resume point".
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

pub fn load_checkpoint(app_data_dir: &Path, run_id: &str) -> Option<RunCheckpoint> {
    let path = checkpoint_path(app_data_dir, run_id)?;
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

pub fn clear_checkpoint(app_data_dir: &Path, run_id: &str) {
    if let Some(path) = checkpoint_path(app_data_dir, run_id) {
        let _ = std::fs::remove_file(path);
    }
}

pub struct SwarmOutcome {
    pub final_answer: String,
    pub plan: Option<SwarmPlan>,
    pub verdict: Option<Verdict>,
    pub tokens_used: usize,
    /// Append-only ledger block for this turn (context for the next turn).
    pub ledger: TurnLedger,
    /// Display-only phase records for history replay. Never sent to the LLM.
    pub transcript: Vec<PhaseRecord>,
}

/// Shared handle to the run's transcript collector. Owned by
/// `agent_swarm_chat` so a failed run can still persist partial phases.
pub type TranscriptCollectorRef = Arc<Mutex<TranscriptCollector>>;

/// Collects `PhaseRecord`s by teeing the same event stream the live UI sees.
/// Mirror of the frontend handler logic (`src/store/agent.ts`) in Rust.
pub struct TranscriptCollector {
    run_id: String,
    records: Vec<PhaseRecord>,
    current: Option<PhaseRecordBuilder>,
}

#[derive(Default)]
struct PhaseRecordBuilder {
    role: String,
    label: String,
    model: String,
    summary: String,
    text: String,
    thinking: String,
    tool_calls: Vec<PhaseToolCall>,
}

impl TranscriptCollector {
    pub fn new(run_id: String) -> Self {
        Self {
            run_id,
            records: Vec::new(),
            current: None,
        }
    }

    pub fn record(&mut self, kind: &AgentEventKind) {
        match kind {
            AgentEventKind::ThoughtDelta(delta) => {
                if let Some(cur) = self.current.as_mut() {
                    cur.text.push_str(delta);
                }
            }
            AgentEventKind::ThinkingDelta(delta) => {
                if let Some(cur) = self.current.as_mut() {
                    cur.thinking.push_str(delta);
                }
            }
            AgentEventKind::ToolCallStarted { call_id, tool_name, arguments_json, .. } => {
                if let Some(cur) = self.current.as_mut() {
                    cur.tool_calls.push(PhaseToolCall {
                        call_id: call_id.clone(),
                        tool_name: tool_name.clone(),
                        arguments_json: arguments_json.clone(),
                        output: String::new(),
                        status: "running".to_string(),
                    });
                }
            }
            AgentEventKind::ToolCallCompleted { call_id, output, .. } => {
                if let Some(cur) = self.current.as_mut() {
                    if let Some(tc) = cur
                        .tool_calls
                        .iter_mut()
                        .find(|t| t.call_id == *call_id)
                    {
                        tc.output = truncate_chars(output, PHASE_TOOL_OUTPUT_CHARS);
                        tc.status = if output.starts_with("Tool execution error") {
                            "error".to_string()
                        } else {
                            "done".to_string()
                        };
                    }
                }
            }
            AgentEventKind::PhaseStarted { role, label, model, .. } => {
                self.finalize_current();
                self.current = Some(PhaseRecordBuilder {
                    role: role.clone(),
                    label: label.clone(),
                    model: model.clone(),
                    ..Default::default()
                });
            }
            AgentEventKind::PhaseCompleted { summary, .. } => {
                if let Some(cur) = self.current.as_mut() {
                    cur.summary = summary.clone();
                }
            }
            AgentEventKind::Finished { .. }
            | AgentEventKind::Error(_)
            | AgentEventKind::ExternalAccessRequest { .. }
            | AgentEventKind::ExternalAccessResolved { .. }
            | AgentEventKind::PlanDecisionRequest { .. }
            | AgentEventKind::PlanDecisionResolved { .. }
            | AgentEventKind::DirectionDecisionRequest { .. }
            | AgentEventKind::DirectionDecisionResolved { .. } => {}
        }
    }

    fn finalize_current(&mut self) {
        if let Some(cur) = self.current.take() {
            self.records.push(PhaseRecord {
                run_id: self.run_id.clone(),
                role: cur.role,
                label: cur.label,
                model: cur.model,
                summary: cur.summary,
                text: cur.text,
                thinking: cur.thinking,
                tool_calls: cur.tool_calls,
                created_at: chrono::Utc::now(),
            });
        }
    }

    /// Finalizes the active phase (if any) and returns all records.
    pub fn finish(&mut self) -> Vec<PhaseRecord> {
        self.finalize_current();
        std::mem::take(&mut self.records)
    }
}

pub struct SwarmOrchestrator {
    inner: AgentOrchestrator,
}

impl SwarmOrchestrator {
    pub fn new(tool_registry: Arc<crate::agent::tool_registry::ToolRegistry>) -> Self {
        Self {
            inner: AgentOrchestrator::new(tool_registry),
        }
    }

    /// Runs the RLM-fronted swarm pipeline over ONE shared, append-only context:
    ///
    /// 0. RLM Phase (cheap, before the expensive Thinker):
    ///    - RLM Model collects curated context into the persistent kernel and
    ///      submits a `brief`. Out-of-project reads trigger interactive approval.
    ///    - RLM Verifier audits the brief for completeness + safety; if gaps are
    ///      found the Model gets ONE more collection round.
    ///    - Only the validated brief digest enters the shared context.
    /// 1. Thinker (slim, expensive) builds a plan directly from the validated brief
    ///    (no exploration) and either answers directly or submits a plan.
    /// 2. Plan Approval Gate (if enabled): the run pauses and waits for the user to
    ///    edit the plan, request a reviewer, or execute. Waiting costs 0 tokens.
    /// 3. Reviewer receives the SAME context (no re-reading) and revises the plan
    ///    (auto-run only when the gate is off).
    /// 4. Executors (Code/Design, cheap models) receive the SAME context + task brief;
    ///    their internal edit turns stay private. Only the resulting diff is appended
    ///    back to the shared context, so the Thinker judges results from diffs only.
    /// 5. Executor Reviewer verifies against the plan; failed verdict spawns one fix round.
    /// 6. Thinker writes the final answer for the user.
    ///
    /// `transcript` is the run's shared transcript collector: every event is teed
    /// into it (alongside the live UI channel) so the display replay survives runs
    /// that fail mid-way.
    pub async fn run_swarm(
        &self,
        messages: &[Message],
        resume: Option<RunCheckpoint>,
        tool_ctx: &ToolContext,
        auto_approve: bool,
        on_event: &Channel<AgentEvent>,
        transcript: &TranscriptCollectorRef,
    ) -> Result<SwarmOutcome> {
        let project_root = tool_ctx.project_root.clone();
        let app_data_dir = tool_ctx.app_data_dir.clone();
        // Run id used to name the resume checkpoint file (mirrors the edit session).
        let run_id = tool_ctx.session_id.clone().unwrap_or_default();
        // Tee: live UI channel first (non-blocking), then the transcript collector.
        // `transcript` is the shared collector passed by `agent_swarm_chat` so a
        // failed run can still persist its partial phases. Mirroring `run_loop`,
        // a closed UI channel is treated as cancellation so the swarm stops
        // burning tokens when nobody is watching.
        let emit = |kind: AgentEventKind| {
            if on_event.send(AgentEvent { kind: kind.clone() }).is_err() {
                tool_ctx.cancel.cancel();
            }
            if let Ok(mut col) = transcript.lock() {
                col.record(&kind);
            }
        };
        let mut total_tokens: usize = resume.as_ref().map(|r| r.total_tokens).unwrap_or(0);
        // Cumulative input/output token split for the whole run (kept separately
        // so the run's final totals can be reported as in/out, while per-phase
        // in/out is emitted on each PhaseCompleted for per-agent cost display).
        let mut total_tokens_in: usize = resume.as_ref().map(|r| r.tokens_in).unwrap_or(0);
        let mut total_tokens_out: usize = resume.as_ref().map(|r| r.tokens_out).unwrap_or(0);
        let mut total_cached_in: usize = resume.as_ref().map(|r| r.cached_in).unwrap_or(0);
        // Ledger accumulator for THIS turn (filled across all phases). On resume
        // the already-produced parts (brief digest) carry over from the checkpoint.
        let mut ledger = resume
            .as_ref()
            .map(|r| r.ledger.clone())
            .unwrap_or_else(|| TurnLedger {
                brief_digest: None,
                plan_markdown: None,
                plan_status: None,
                execution_review: None,
                final_answer: String::new(),
            });
        // Condensed executor notes (`[EXEC] Task #N ...`) for the ledger.
        // On an Executing resume the notes from completed tasks carry over.
        let mut exec_notes: Vec<String> = resume
            .as_ref()
            .map(|r| r.exec_notes.clone())
            .unwrap_or_default();

        // Shared swarm context: every role gets this history, append-only.
        // The RLM phase appends the validated brief digest here; the raw
        // exploration of the RLM Model stays private (never enters `shared`).
        // On resume, `shared` already contains the brief digest + direction the
        // failed run produced, so the completed phases are NOT re-run.
        let mut shared: Vec<Message> = match &resume {
            Some(r) => r.shared.clone(),
            None => messages.to_vec(),
        };

        // Provider config is needed later (plan gate) regardless of resume state.
        let cfg = ProviderConfigManager::new(&app_data_dir)?.load()?;
        let gate_enabled = cfg.agent.plan_gate_enabled;

        // Resume boundaries: Phase 0 (RLM) runs only on a fresh run — its output
        // (validated brief digest) is already inside `shared` from the checkpoint.
        // Phase 0.5 (Thinker direction + user review) ALSO runs when a run stopped
        // at the direction boundary: the checkpoint captured `shared` right after
        // RLM, and the direction conclusion + gate were never produced.
        let resume_direction = matches!(
            &resume,
            Some(RunCheckpoint { phase: ResumePhase::Direction, .. })
        );

        // ── Phase 0: RLM Phase (cheap collector + verifier, before Thinker) ─
        if resume.is_none() {
        // The RLM Model's exploration turns accumulate in a PRIVATE context
        // (`rlm_ctx`); only the validated brief digest is pushed to `shared`.
        let mut rlm_ctx: Vec<Message> = messages.to_vec();
        let mut brief_text: Option<String> = None;

        // ── Phase 0 setup: RLM cache (per-project, stored outside the project) ─
        // A new chat reuses the last validated research when the tree is
        // unchanged (sufficiency check) or lightly changed (incremental refresh),
        // instead of re-researching from zero every time.
        let cache_store = rlm_cache::RlmCacheStore::new(&app_data_dir);
        let cached = cache_store.load(&project_root);
        let current_manifest = match rlm_cache::build_manifest(
            &project_root,
            cached.as_ref().and_then(|c| c.manifest.as_ref()),
        ) {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::warn!("RLM manifest build failed; treating cache as fresh: {}", e);
                None
            }
        };
        let manifest_diff =
            match (cached.as_ref().and_then(|c| c.manifest.as_ref()), current_manifest.as_ref()) {
                (Some(old), Some(new)) => Some(rlm_cache::diff_manifest(old, new)),
                _ => None,
            };
        let cache_decision = rlm_cache::classify_cache_state(
            &project_root,
            cached.as_ref(),
            manifest_diff.as_ref(),
            CHANGED_RATIO_THRESHOLD,
            chrono::Duration::days(MAX_BRIEF_AGE_DAYS),
        );

        // Pre-warm the kernel with the previous inventory so the model does not
        // re-read known-good files. Changed files are never pre-warmed — the
        // model must read their current state explicitly.
        match &cache_decision {
            CacheDecision::Sufficiency => prewarm_cache(&cached, &manifest_diff, &project_root).await,
            CacheDecision::Incremental => prewarm_cache(&cached, &manifest_diff, &project_root).await,
            CacheDecision::Fresh => {}
        }

        let user_query = messages
            .last()
            .map(|m| m.content.clone())
            .unwrap_or_default();
        let mut final_brief: Option<ResearchBrief> = None;
        let mut final_audit: Option<ContextAudit> = None;
        // Audit from the previous round, so round 2+ can run a DELTA audit that
        // re-checks ONLY the previously-missing items instead of re-auditing the
        // whole brief (token savings) while still confirming each gap is really filled.
        let mut prev_audit: Option<ContextAudit> = None;

        for rlm_round in 0..MAX_RLM_ROUNDS {
            let is_first_round = rlm_round == 0;
            let sufficiency_round = is_first_round
                && matches!(&cache_decision, CacheDecision::Sufficiency);
            let incremental_round =
                is_first_round && matches!(&cache_decision, CacheDecision::Incremental);

            if sufficiency_round {
                if let Some(c) = cached.as_ref() {
                    if let Some(digest) = c.digest.as_ref() {
                        rlm_ctx.push(Message::user(format!(
                            "[PRIOR RESEARCH — generated {}{}]\n{}\n\n\
                             SUFFICIENCY CHECK: the research above was validated in a previous session \
                             and its data is already loaded into the kernel. Evaluate whether it is \
                             sufficient for the current user request: {}\n\
                             - SUFFICIENT → call submit_brief now (you may refine minor details).\n\
                             - INSUFFICIENT → collect ONLY the missing pieces, then call submit_brief.",
                            format_ts(c.manifest.as_ref().map(|m| m.generated_at)),
                            label_prior_research(c),
                            digest,
                            user_query
                        )));
                    }
                }
            } else if incremental_round {
                if let (Some(c), Some(d)) = (cached.as_ref(), manifest_diff.as_ref()) {
                    if let Some(digest) = c.digest.as_ref() {
                        let changed_list = d.all_changed();
                        rlm_ctx.push(Message::user(format!(
                            "[PRIOR RESEARCH — generated {}{}]\n{}\n\n\
                             INCREMENTAL CHECK: the research above is from a previous session. \
                             These files changed in the project since then:\n{}\n\
                             Collect/refresh ONLY what is relevant to the current user request, \
                             then call submit_brief.",
                            format_ts(c.manifest.as_ref().map(|m| m.generated_at)),
                            label_prior_research(c),
                            digest,
                            changed_list.join("\n")
                        )));
                    }
                }
            } else if is_first_round {
                // Fresh research: offer the old brief (if any) as an explicitly
                // STALE reference only — never as ground truth (anti-anchoring).
                if let Some(c) = cached.as_ref() {
                    if let Some(digest) = c.digest.as_ref() {
                        rlm_ctx.push(Message::user(format!(
                            "[PRIOR RESEARCH — STALE REFERENCE — generated {}{}. This research is \
                             OUTDATED (the project changed significantly or too much time passed). Do \
                             NOT trust it as-is; use it only as a rough starting map. Collect the \
                             CURRENT state yourself, then call submit_brief.]\n{}",
                            format_ts(c.manifest.as_ref().map(|m| m.generated_at)),
                            label_prior_research(c),
                            digest
                        )));
                    }
                }
            }

            // ── 0a. RLM Model: collect curated context into the kernel ───
            let model_provider = resolve_role_provider(AgentRole::RlmModel, &app_data_dir).await?;
            let round_label = if sufficiency_round {
                "RLM Model: sufficiency check (cached)".to_string()
            } else if incremental_round {
                "RLM Model: incremental refresh (cached)".to_string()
            } else if rlm_round == 0 {
                "RLM Model: collecting context".to_string()
            } else {
                format!("RLM Model: collecting context (round {})", rlm_round + 1)
            };
            emit(AgentEventKind::PhaseStarted {
                role: AgentRole::RlmModel.key().to_string(),
                label: round_label,
                model: model_provider.name().to_string(),
            });

            let model_spec = AgentRole::RlmModel.spec();
            let model_max_turns = if sufficiency_round {
                SUFFICIENCY_MAX_TURNS
            } else {
                model_spec.max_turns
            };
            let model_outcome = self
                .inner
                .run_role_loop(
                    RoleLoopParams {
                        system_prompt: PromptComposer::compose_role_prompt(AgentRole::RlmModel, &project_root),
                        messages: rlm_ctx.clone(),
                        allowed_tools: &model_spec.allowed_tools,
                        provider: model_provider,
                        max_turns: model_max_turns,
                        temperature: model_spec.temperature,
                        stop_on_tool: Some("submit_brief"),
                        role_name: AgentRole::RlmModel.key(),
                    },
                    tool_ctx,
                    auto_approve,
                    &emit,
                )
                .await?;
            total_tokens += model_outcome.tokens_used;
            total_tokens_in += model_outcome.tokens_in;
            total_cached_in += model_outcome.cached_in;
            total_tokens_out += model_outcome.tokens_out;
            rlm_ctx = model_outcome.history;

            let (raw_brief_text, brief) = match &model_outcome.stop_tool_args {
                Some(args) => match handoff_doc(&project_root, args, &model_outcome.final_text) {
                    Ok(doc) => {
                        // Expand `[SNIPPET id="N"]` placeholders into verbatim
                        // `--- path [start-end]` blocks pulled from the RLM kernel,
                        // so the brief's code is byte-exact (never retyped by the model).
                        let doc = resolve_snippet_placeholders(&project_root, &doc).await;
                        // Make the expanded brief the model's final visible message —
                        // the user and the RLM Verifier should see the ACTUAL code,
                        // not `[SNIPPET id="N"]` placeholders.
                        if let Some(last) = rlm_ctx
                            .iter_mut()
                            .rev()
                            .find(|m| m.role == MessageRole::Assistant)
                        {
                            last.content = doc.clone();
                        }
                        // Keep the on-disk artifact consistent with the expanded brief.
                        persist_handoff_artifact(&project_root, args, &doc);
                        let brief = parse_brief_doc(&doc).unwrap_or_else(|_| ResearchBrief {
                            summary: doc.clone(),
                            ..Default::default()
                        });
                        (Some(doc), brief)
                    }
                    Err(_) => (None, ResearchBrief {
                        summary: model_outcome.final_text.clone(),
                        ..Default::default()
                    }),
                },
                None => {
                    let disk_brief = project_root.join(".kuda").join("brief.md");
                    let disk_content = std::fs::read_to_string(&disk_brief).ok().filter(|s| !s.trim().is_empty());
                    let text = disk_content.unwrap_or_else(|| model_outcome.final_text.clone());
                    let brief = parse_brief_doc(&text).unwrap_or_else(|_| ResearchBrief {
                        summary: text.clone(),
                        ..Default::default()
                    });
                    (Some(text), brief)
                }
            };

            emit(AgentEventKind::PhaseCompleted {
                role: AgentRole::RlmModel.key().to_string(),
                summary: if model_outcome.stop_tool_args.is_some() {
                    "Brief submitted".to_string()
                } else if model_outcome.exhausted_turns {
                    "Ended without submit_brief (turn limit; using final text)".to_string()
                } else {
                    "Ended without submit_brief (using final text)".to_string()
                },
                tokens_in: model_outcome.tokens_in,
                tokens_out: model_outcome.tokens_out,
                cached_in: model_outcome.cached_in,
            });

            // ── 0b. RLM Verifier: audit the brief for completeness + safety ─
            let verifier_provider = resolve_role_provider(AgentRole::RlmVerifier, &app_data_dir).await?;
            emit(AgentEventKind::PhaseStarted {
                role: AgentRole::RlmVerifier.key().to_string(),
                label: "RLM Verifier: auditing brief".to_string(),
                model: verifier_provider.name().to_string(),
            });

            let rendered_digest = format_brief_digest(
                &brief,
                &ContextAudit {
                    complete: false,
                    summary: String::new(),
                    missing: vec![],
                },
            );
            let brief_doc = raw_brief_text.as_deref().unwrap_or(&rendered_digest);
            let mut verify_ctx = rlm_ctx.clone();
            if rlm_round == 0 {
                verify_ctx.push(Message::user(format!(
                    "[SWARM] RLM Verifier step. Submitted brief (markdown):\n{}\nVerify the research \
                     above is correct and complete for the request, that external pulls are safe, \
                     then write the audit as markdown text and call submit_audit exactly once.",
                    brief_doc
                )));
            } else {
                // DELTA audit: the previous round's gaps were fed back to the RLM
                // Model, which re-submitted the brief. Re-verify ONLY those items
                // instead of re-auditing the whole brief. Anything not confirmed
                // as genuinely filled must still surface as a gap.
                let prev_missing = prev_audit
                    .as_ref()
                    .map(|a| {
                        a.missing
                            .iter()
                            .map(|g| {
                                format!(
                                    "- {} ({}) — needed for {}",
                                    g.what, g.where_path, g.why_needed
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                verify_ctx.push(Message::user(format!(
                    "[SWARM] RLM VERIFIER — DELTA AUDIT (round {}) of the re-submitted brief. \
                     The previous audit found these gaps:\n{}\n\
                     The RLM Model was asked to fill ONLY those gaps. Confirm each item is now \
                     genuinely present and correct: run a `grep_search`/`code_outline` (or \
                     `_rlm_symbols` via `rlm_python`) per item before you mark it resolved. \
                     Do NOT re-audit the whole brief. Any item still missing or now-incorrect goes \
                     into ## Missing; once every previous gap is confirmed filled, set the audit \
                     to COMPLETE. Revised brief (markdown):\n{}",
                    rlm_round, prev_missing, brief_doc
                )));
            }

            let verifier_spec = AgentRole::RlmVerifier.spec();
            let verifier_prompt =
                PromptComposer::compose_role_prompt(AgentRole::RlmVerifier, &project_root);
            let verifier_outcome = self
                .inner
                .run_role_loop(
                    RoleLoopParams {
                        system_prompt: verifier_prompt.clone(),
                        messages: verify_ctx,
                        allowed_tools: &verifier_spec.allowed_tools,
                        provider: verifier_provider.clone(),
                        max_turns: verifier_spec.max_turns,
                        temperature: verifier_spec.temperature,
                        stop_on_tool: Some("submit_audit"),
                        role_name: AgentRole::RlmVerifier.key(),
                    },
                    tool_ctx,
                    auto_approve,
                    &emit,
                )
                .await?;
            total_tokens += verifier_outcome.tokens_used;
            total_tokens_in += verifier_outcome.tokens_in;
            total_cached_in += verifier_outcome.cached_in;
            total_tokens_out += verifier_outcome.tokens_out;
            // This phase's own totals (include any audit re-submit below).
            let mut verifier_phase_tokens_in = verifier_outcome.tokens_in;
            let mut verifier_phase_cached_in = verifier_outcome.cached_in;
            let mut verifier_phase_tokens_out = verifier_outcome.tokens_out;

            // An unparseable audit is the verifier misbehaving, not a real context
            // gap — so retry the VERIFIER once instead of asking the RLM Model to
            // "fill in" a gap that does not exist.
            let audit = match &verifier_outcome.stop_tool_args {
                Some(args) => match handoff_doc(&project_root, args, &verifier_outcome.final_text) {
                    Ok(doc) => match parse_audit_doc(&doc) {
                        Ok(a) => a,
                        Err(_) => {
                            tracing::warn!("RLM Verifier audit document invalid; requesting one re-submit.");
                            let mut retry_ctx = verifier_outcome.history.clone();
                            retry_ctx.push(Message::user(
                                "[RLM AUDIT] Your previous audit document could not be parsed. \
                                 Write the corrected audit as markdown text in your response \
                                 (template: \"# Audit: COMPLETE\" followed by your assessment, and \
                                 a \"## Missing\" section of \"- what — where (needed for: why)\" \
                                 lines when incomplete), then call submit_audit exactly once again \
                                 with {\"file_path\": \".kuda/audit.md\"}.",
                            ));
                            let retry_outcome = self
                                .inner
                                .run_role_loop(
                                    RoleLoopParams {
                                        system_prompt: verifier_prompt,
                                        messages: retry_ctx,
                                        allowed_tools: &verifier_spec.allowed_tools,
                                        provider: verifier_provider,
                                        max_turns: RLM_VERIFIER_RETRY_TURNS,
                                        temperature: verifier_spec.temperature,
                                        stop_on_tool: Some("submit_audit"),
                                        role_name: "rlm_verifier (audit re-submit)",
                                    },
                                    tool_ctx,
                                    auto_approve,
                                    &emit,
                                )
                                .await?;
                            total_tokens += retry_outcome.tokens_used;
                            total_tokens_in += retry_outcome.tokens_in;
                            total_cached_in += retry_outcome.cached_in;
                            total_tokens_out += retry_outcome.tokens_out;
                            verifier_phase_tokens_in += retry_outcome.tokens_in;
                            verifier_phase_cached_in += retry_outcome.cached_in;
                            verifier_phase_tokens_out += retry_outcome.tokens_out;
                            match &retry_outcome.stop_tool_args {
                                Some(args2) => match handoff_doc(&project_root, args2, &retry_outcome.final_text) {
                                    Ok(doc2) => parse_audit_doc(&doc2).unwrap_or_else(|_| ContextAudit {
                                        complete: false,
                                        summary: "RLM Verifier re-submitted an unparseable audit; brief treated as incomplete.".to_string(),
                                        missing: vec![AuditGap {
                                            what: "RLM Verifier failed to produce a valid audit".into(),
                                            where_path: String::new(),
                                            why_needed: "verification of research completeness".into(),
                                        }],
                                    }),
                                    Err(_) => ContextAudit {
                                        complete: false,
                                        summary: "RLM Verifier re-submitted an empty audit; brief unverified.".to_string(),
                                        missing: vec![AuditGap {
                                            what: "RLM Verifier failed to produce an audit document".into(),
                                            where_path: String::new(),
                                            why_needed: "verification of research completeness".into(),
                                        }],
                                    },
                                },
                                None => ContextAudit {
                                    complete: false,
                                    summary: "RLM Verifier did not re-submit an audit; brief unverified.".to_string(),
                                    missing: vec![AuditGap {
                                        what: "RLM Verifier did not confirm the brief".into(),
                                        where_path: String::new(),
                                        why_needed: "verification of research completeness".into(),
                                    }],
                                },
                            }
                        }
                    },
                    Err(_) => ContextAudit {
                        complete: false,
                        summary: "RLM Verifier did not produce an audit document; brief unverified.".to_string(),
                        missing: vec![AuditGap {
                            what: "RLM Verifier did not confirm the brief".into(),
                            where_path: String::new(),
                            why_needed: "verification of research completeness".into(),
                        }],
                    },
                },
                None => ContextAudit {
                    complete: false,
                    summary: if verifier_outcome.exhausted_turns {
                        "RLM Verifier hit its turn limit without submitting an audit; brief unverified.".to_string()
                    } else {
                        "RLM Verifier did not submit an audit; brief unverified.".to_string()
                    },
                    missing: vec![AuditGap {
                        what: "RLM Verifier did not confirm the brief".into(),
                        where_path: String::new(),
                        why_needed: "verification of research completeness".into(),
                    }],
                },
            };
            // Record this round's audit so the next round (if any) can run a
            // DELTA audit scoped to the still-open gaps.
            prev_audit = Some(audit.clone());
            emit(AgentEventKind::PhaseCompleted {
                role: AgentRole::RlmVerifier.key().to_string(),
                summary: if audit.complete {
                    "Brief complete & safe".to_string()
                } else {
                    format!("Missing {} item(s)", audit.missing.len())
                },
                tokens_in: verifier_phase_tokens_in,
                tokens_out: verifier_phase_tokens_out,
                cached_in: verifier_phase_cached_in,
            });

            if audit.complete || rlm_round + 1 >= MAX_RLM_ROUNDS {
                let disk_brief = project_root.join(".kuda").join("brief.md");
                let disk_content = std::fs::read_to_string(&disk_brief).ok().filter(|s| !s.trim().is_empty());
                brief_text = raw_brief_text
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .or(disk_content)
                    .or_else(|| Some(format_brief_digest(&brief, &audit)));
                if !audit.complete {
                    rlm_ctx.push(Message::user(format!(
                        "[RLM AUDIT] Incomplete but no more rounds allowed. Missing: {}",
                        audit
                            .missing
                            .iter()
                            .map(|g| format!("{} ({})", g.what, g.where_path))
                            .collect::<Vec<_>>()
                            .join("; ")
                    )));
                }
                final_brief = Some(brief);
                final_audit = Some(audit);
                break;
            }

            // Gap → ask the RLM Model to fill it in one more round.
            // Continue directly from the verifier's outcome history so prompt cache is 100% preserved.
            rlm_ctx = verifier_outcome.history;
            let missing_note = audit
                .missing
                .iter()
                .map(|g| format!("- {} ({}): needed for {}", g.what, g.where_path, g.why_needed))
                .collect::<Vec<_>>()
                .join("\n");
            rlm_ctx.push(Message::user(format!(
                "[RLM AUDIT] {} Missing research:\n{}\nCollect ONLY these gaps, then call submit_brief again.",
                audit.summary, missing_note
            )));
        }

        // Push the validated brief digest into the shared context for the Thinker.
        let brief_digest = brief_text.unwrap_or_else(|| {
            "(No RLM brief produced — proceed with caution.)".to_string()
        });
        ledger.brief_digest = Some(truncate_chars(&brief_digest, LEDGER_BRIEF_CHARS));
        // The digest's timestamp should reflect when the brief's DATA originates:
        // - Sufficiency: the brief is genuinely reused from the cached research,
        //   so its real generation time is the cached manifest's timestamp.
        // - Incremental / Fresh: the brief was (re)produced this run, so "now".
        let research_ts = match &cache_decision {
            CacheDecision::Sufficiency => cached
                .as_ref()
                .and_then(|c| c.manifest.as_ref())
                .map(|m| m.generated_at)
                .unwrap_or_else(chrono::Local::now),
            _ => chrono::Local::now(),
        };
        // A brief that never passed the verifier must not be presented as
        // validated ground truth — mark it loudly so the Thinker (and any
        // reviewer) knows it is unreliable and cannot plan on "no code".
        let incomplete_note = if matches!(final_audit.as_ref(), Some(a) if !a.complete) {
            format!(
                "\n[!] WARNING: this brief did NOT pass the RLM Verifier ({} missing item(s): {}) — treat its claims as unverified.",
                final_audit.as_ref().map(|a| a.missing.len()).unwrap_or(0),
                final_audit
                    .as_ref()
                    .map(|a| {
                        a.missing
                            .iter()
                            .map(|g| g.what.clone())
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .unwrap_or_default()
            )
        } else {
            String::new()
        };
        shared.push(Message::user(format!(
            "[RESEARCH BRIEF — validated by RLM, generated {}]{}\n{}",
            format_ts(Some(research_ts)),
            incomplete_note,
            brief_digest
        )));

        // Persist the validated research for the next chat on this project, so
        // a new chat can run a sufficiency check instead of re-researching.
        // Only a genuinely verified brief is cached; failures are non-fatal.
        if let (Some(brief), Some(audit)) = (final_brief, final_audit) {
            if audit.complete {
                if let Some(manifest) = current_manifest.as_ref() {
                    let inventory = get_rlm_manager().inventory_snapshot(&project_root).await;
                    if let Err(e) = cache_store.save(
                        &project_root,
                        &brief,
                        &audit,
                        &brief_digest,
                        manifest,
                        &inventory,
                    ) {
                        tracing::warn!("Failed to persist RLM research cache: {}", e);
                    }
                }
            }
        }

        // Checkpoint after RLM research: a failure in any later phase can resume
        // here without re-collecting or re-verifying the research.
        write_checkpoint(
            &app_data_dir,
            &run_id,
            &RunCheckpoint {
                phase: ResumePhase::Direction,
                shared: shared.clone(),
                total_tokens,
                tokens_in: total_tokens_in,
                tokens_out: total_tokens_out,
                cached_in: total_cached_in,
                ledger: ledger.clone(),
                final_plan: None,
                pending_tasks: Vec::new(),
                executor_logs: Vec::new(),
                fix_round: 0,
                exec_notes: Vec::new(),
            },
        );
        } // end Phase 0 (RLM) — skipped on ANY resume; the validated brief digest
          // is already in `shared` from the checkpoint.

        // ── Phase 0.5: Thinker direction checkpoint (kesimpulan sementara) ──
        // Runs on a fresh run AND on a resume that stopped at the direction
        // boundary (`shared` then already contains the validated brief digest).
        let mut thinker_history: Vec<Message> = Vec::new();
        if resume.is_none() || resume_direction {
        // Before the expensive full plan, the Thinker writes a SHORT temporary
        // conclusion in the agent window. When the plan gate is enabled the run
        // PAUSES for a brief user review ("lanjut" / "ubah" + note), so a wrong
        // direction is corrected before the full detailed plan is written. For
        // pure explanation requests the conclusion begins with NO_FILE_CHANGES
        // and becomes the final answer directly (no pause, no full plan).
        //
        // The whole direction phase sits in a bounded review loop: choosing
        // "ubah" + note re-runs the Thinker conclusion with the note folded in.
        // After `MAX_DIRECTION_REVISIONS` revisions a further "ubah" FAILS
        // CLOSED (consistent with the plan gate) instead of silently treating
        // the rejection as an approval.
        const MAX_DIRECTION_REVISIONS: usize = 2;
        let mut direction_revisions = 0usize;
        let mut direction_note: Option<String> = None;
        'direction_review: loop {
        let thinker_provider = resolve_role_provider(AgentRole::Thinker, &app_data_dir).await?;
        emit(AgentEventKind::PhaseStarted {
            role: AgentRole::Thinker.key().to_string(),
            label: "Thinker: kesimpulan sementara".to_string(),
            model: thinker_provider.name().to_string(),
        });
        let direction_prompt = format!(
            "{}\n\nDIRECTION CHECKPOINT (STAGE A — FIRST): you are asked ONLY for your \
             temporary conclusion right now. Do NOT write .kuda/plan.md, do NOT call \
             submit_plan. You rely 100% on the validated research brief. If the plan needs \
             a concrete fact, config value, or file content the brief lacks, call request_rlm_research \
             (at most twice per run) so the RLM researcher collects it for you. Write a TEMPORARY \
             CONCLUSION as your response text: restate the goal in one line, summarize the \
             chosen approach in 2-4 short bullets, list the main files to be touched, and \
             note key risks/assumptions. The user reads this in the agent window and approves \
             your direction before you create the full plan. If the request needs NO file \
             changes, begin your conclusion with exactly: NO_FILE_CHANGES",
            PromptComposer::compose_role_prompt(AgentRole::Thinker, &project_root)
        );

        let mut dir_history: Vec<Message> = shared.clone();
        let mut dir_phase_tokens_in: usize = 0;
        let mut dir_phase_tokens_out: usize = 0;
        let mut dir_phase_cached_in: usize = 0;
        let mut research_requests = 0usize;
        let dir_outcome = loop {
            let outcome = self
                .inner
                .run_role_loop(
                    RoleLoopParams {
                        system_prompt: direction_prompt.clone(),
                        messages: dir_history.clone(),
                        allowed_tools: &[
                            "request_rlm_research".to_string(),
                        ],
                        provider: thinker_provider.clone(),
                        max_turns: 3,
                        temperature: 0.2,
                        stop_on_tool: Some("request_rlm_research"),
                        role_name: "thinker (kesimpulan sementara)",
                    },
                    tool_ctx,
                    auto_approve,
                    &emit,
                )
                .await?;
            total_tokens += outcome.tokens_used;
            total_tokens_in += outcome.tokens_in;
            total_cached_in += outcome.cached_in;
            total_tokens_out += outcome.tokens_out;
            dir_phase_tokens_in += outcome.tokens_in;
            dir_phase_tokens_out += outcome.tokens_out;
            dir_phase_cached_in += outcome.cached_in;

            let Some(research_args) = outcome.stop_tool_args.clone() else {
                break outcome;
            };

            if research_requests >= MAX_THINKER_RESEARCH_REQUESTS {
                let mut conclude_history = outcome.history.clone();
                conclude_history.push(Message::user(
                    "[RLM] No more research rounds are available for this run. Note anything \
                     still unknown under Risks/Unknowns and write your TEMPORARY CONCLUSION now."
                        .to_string(),
                ));
                let concluded = self
                    .inner
                    .run_role_loop(
                        RoleLoopParams {
                            system_prompt: direction_prompt.clone(),
                            messages: conclude_history,
                            allowed_tools: &["batch_file_read".to_string()],
                            provider: thinker_provider.clone(),
                            max_turns: 2,
                            temperature: 0.2,
                            stop_on_tool: None,
                            role_name: "thinker (kesimpulan sementara)",
                        },
                        tool_ctx,
                        auto_approve,
                        &emit,
                    )
                    .await?;
                total_tokens += concluded.tokens_used;
                total_tokens_in += concluded.tokens_in;
                total_cached_in += concluded.cached_in;
                total_tokens_out += concluded.tokens_out;
                dir_phase_tokens_in += concluded.tokens_in;
                dir_phase_tokens_out += concluded.tokens_out;
                dir_phase_cached_in += concluded.cached_in;
                break concluded;
            }

            let (supplement, sup_in, sup_out, sup_cached) = self
                .run_rlm_research_round(
                    &dir_history,
                    &research_args,
                    tool_ctx,
                    auto_approve,
                    &emit,
                )
                .await?;
            total_tokens += sup_in + sup_out;
            total_tokens_in += sup_in;
            total_cached_in += sup_cached;
            total_tokens_out += sup_out;

            let supplement_msg = Message::user(format!(
                "[RESEARCH SUPPLEMENT — dikumpulkan RLM atas permintaan Thinker — tidak \
                 diverifikasi — {}]\n{}",
                format_ts(Some(chrono::Local::now())),
                supplement
            ));
            shared.push(supplement_msg.clone());
            dir_history = outcome.history.clone();
            dir_history.push(supplement_msg);
            research_requests += 1;
        };
        thinker_history = dir_outcome.history.clone();
        let conclusion = dir_outcome.final_text.trim().to_string();
        if conclusion.is_empty() {
            emit(AgentEventKind::PhaseCompleted {
                role: AgentRole::Thinker.key().to_string(),
                summary: "Thinker tidak menghasilkan teks kesimpulan sementara (berhenti di pemikiran)".to_string(),
                tokens_in: dir_phase_tokens_in,
                tokens_out: dir_phase_tokens_out,
                cached_in: dir_phase_cached_in,
            });
            return Err(AppError::General(
                "Thinker tidak menghasilkan output kesimpulan sementara (berhenti di pemikiran)".to_string(),
            ));
        }
        emit(AgentEventKind::PhaseCompleted {
            role: AgentRole::Thinker.key().to_string(),
            summary: format!(
                "Kesimpulan sementara ({} karakter)",
                conclusion.chars().count()
            ),
            tokens_in: dir_phase_tokens_in,
            tokens_out: dir_phase_tokens_out,
            cached_in: dir_phase_cached_in,
        });

        // NO_FILE_CHANGES marker → the conclusion IS the final answer.
        let is_no_change = conclusion.starts_with("NO_FILE_CHANGES");
        if is_no_change {
            let answer = conclusion["NO_FILE_CHANGES".len()..].trim().to_string();
            if !answer.is_empty() {
                emit(AgentEventKind::Finished {
                    total_tokens_used: total_tokens,
                    tokens_in: total_tokens_in,
                    tokens_out: total_tokens_out,
                    cached_in: total_cached_in,
                });
                return Ok(SwarmOutcome {
                    final_answer: answer.clone(),
                    plan: None,
                    verdict: None,
                    tokens_used: total_tokens,
                    ledger: TurnLedger {
                        brief_digest: ledger.brief_digest.clone(),
                        plan_markdown: None,
                        plan_status: None,
                        execution_review: None,
                        final_answer: truncate_chars(&answer, LEDGER_ANSWER_CHARS),
                    },
                    transcript: transcript.lock().map(|mut c| c.finish()).unwrap_or_default(),
                });
            }
        }

        // Brief user review of the direction (only when the plan gate is on).
        if gate_enabled && !conclusion.is_empty() && !is_no_change {
            let request_id = format!("direction_{}", uuid::Uuid::new_v4().simple());
            let rx = tool_ctx.direction_decisions.register(&request_id);
            emit(AgentEventKind::DirectionDecisionRequest {
                request_id: request_id.clone(),
                conclusion: conclusion.clone(),
            });
            let decision = tokio::select! {
                r = rx => match r {
                    Ok(d) => d,
                    // Sender dropped (run cancelled / stale entry purged): fail
                    // CLOSED — never auto-approve the direction by defaulting
                    // to "lanjut" (this was inconsistent with the plan gate).
                    Err(_) => ("cancelled".to_string(), None),
                },
                _ = tool_ctx.cancel.notified() => ("cancelled".to_string(), None),
            };
            // The `resolve` path already removed the entry; this drops it on the
            // cancel/timeout path so a stale sender can never be resolved later.
            tool_ctx.direction_decisions.remove(&request_id);
            emit(AgentEventKind::DirectionDecisionResolved {
                request_id,
                decision: decision.0.clone(),
                note: decision.1.clone(),
            });
            match decision.0.as_str() {
                "cancelled" => {
                    let final_text =
                        "(dibatalkan user pada review kesimpulan sementara)".to_string();
                    ledger.plan_status = Some("CANCELLED AT DIRECTION".to_string());
                    ledger.final_answer = truncate_chars(&final_text, LEDGER_ANSWER_CHARS);
                    emit(AgentEventKind::Finished {
                        total_tokens_used: total_tokens,
                        tokens_in: total_tokens_in,
                        tokens_out: total_tokens_out,
                        cached_in: total_cached_in,
                    });
                    return Ok(SwarmOutcome {
                        final_answer: final_text,
                        plan: None,
                        verdict: None,
                        tokens_used: total_tokens,
                        ledger,
                        transcript: transcript.lock().map(|mut c| c.finish()).unwrap_or_default(),
                    });
                }
                "ubah" => {
                    direction_revisions += 1;
                    if direction_revisions >= MAX_DIRECTION_REVISIONS {
                        // Revision budget exhausted: fail CLOSED — never proceed
                        // to the full plan on a direction the user kept rejecting.
                        let final_text =
                            "(arah tidak disetujui setelah batas revisi — run dibatalkan)"
                                .to_string();
                        ledger.plan_status = Some("CANCELLED AT DIRECTION".to_string());
                        ledger.final_answer = truncate_chars(&final_text, LEDGER_ANSWER_CHARS);
                        emit(AgentEventKind::Finished {
                            total_tokens_used: total_tokens,
                            tokens_in: total_tokens_in,
                            tokens_out: total_tokens_out,
                            cached_in: total_cached_in,
                        });
                        return Ok(SwarmOutcome {
                            final_answer: final_text,
                            plan: None,
                            verdict: None,
                            tokens_used: total_tokens,
                            ledger,
                            transcript: transcript.lock().map(|mut c| c.finish()).unwrap_or_default(),
                        });
                    }
                    let note = decision.1.unwrap_or_default();
                    let revision_msg = if note.trim().is_empty() {
                        "[USER DIRECTION REVISION] User meminta arah diubah. Tulis ulang \
                         kesimpulan sementara Anda sesuai permintaan."
                            .to_string()
                    } else {
                        format!(
                            "[USER DIRECTION REVISION] User meminta arah diubah dengan catatan: \
                             {}\nPerbaiki kesimpulan sementara Anda sesuai catatan ini.",
                            note.trim()
                        )
                    };
                    shared.push(Message::user(revision_msg));
                    // Refresh the Direction checkpoint with the revision folded
                    // in: a crash during a later revision would otherwise resume
                    // from the pre-revision state and silently discard the
                    // user's earlier revision notes (re-generating a fresh,
                    // different direction and re-spending tokens).
                    write_checkpoint(
                        &app_data_dir,
                        &run_id,
                        &RunCheckpoint {
                            phase: ResumePhase::Direction,
                            shared: shared.clone(),
                            total_tokens,
                            tokens_in: total_tokens_in,
                            tokens_out: total_tokens_out,
                            cached_in: total_cached_in,
                            ledger: ledger.clone(),
                            final_plan: None,
                            pending_tasks: Vec::new(),
                            executor_logs: Vec::new(),
                            fix_round: 0,
                            exec_notes: Vec::new(),
                        },
                    );
                    continue 'direction_review;
                }
                _ => {
                    // "lanjut" (approve) or any unknown decision: proceed with the
                    // (possibly empty) note folded into the full-plan instruction.
                    direction_note = decision.1;
                }
            }
        }
        // Fold the approved direction (and any user note) into the context for
        // the full-plan stage so the Thinker does not lose the user's adjustment
        // or contradict the direction it just got approved.
        let direction_ctx = match direction_note {
            Some(note) if !note.trim().is_empty() => format!(
                "[USER DIRECTION] Arah disetujui dengan catatan: {}\n",
                note.trim()
            ),
            _ => "[USER DIRECTION] Arah disetujui.\n".to_string(),
        };
        let direction_ctx = if conclusion.is_empty() {
            direction_ctx
        } else {
            format!(
                "{}[DIRECTION CONCLUSION — the user approved THIS direction; the full plan \
                 must implement it, not contradict it]:\n{}\n",
                direction_ctx, conclusion
            )
        };
        let prompt_to_writer = format!(
            "{}Sekarang buat FULL plan ke .kuda/plan.md lalu panggil submit_plan.",
            direction_ctx
        );
        shared.push(Message::user(prompt_to_writer.clone()));
        thinker_history.push(Message::user(prompt_to_writer));
        break 'direction_review;
        } // end `'direction_review` review loop (direction approved, or gate off)

        // Checkpoint after the direction was approved: a failure in the planning
        // phase (or any later phase) resumes here — reusing the validated brief
        // AND the approved direction, so only the full plan needs to be produced.
        write_checkpoint(
            &app_data_dir,
            &run_id,
            &RunCheckpoint {
                phase: ResumePhase::Planning,
                shared: shared.clone(),
                total_tokens,
                tokens_in: total_tokens_in,
                tokens_out: total_tokens_out,
                cached_in: total_cached_in,
                ledger: ledger.clone(),
                final_plan: None,
                pending_tasks: Vec::new(),
                executor_logs: Vec::new(),
                fix_round: 0,
                exec_notes: Vec::new(),
            },
        );
        } // end Phase 0.5 (direction) — skipped only when resuming from Planning
          // or Executing (the direction conclusion is already in `shared`).

        // A resume mid-execution skips planning + the plan gate too: the approved
        // plan and completed-task context come from the checkpoint.
        let resume_executing = matches!(
            &resume,
            Some(RunCheckpoint { phase: ResumePhase::Executing, .. })
        );
        // Approved plan for the execution phase (from the gate on a fresh run,
        // or from the checkpoint when resuming mid-execution).
        let mut approved_plan: Option<SwarmPlan> = if resume_executing {
            resume.as_ref().and_then(|r| r.final_plan.clone())
        } else {
            None
        };

        if !resume_executing {
        // ── Phase 1: Planning Writer loop (cheap writer + Thinker review) ─
        // The expensive Thinker already wrote only a SHORT temporary conclusion
        // in the direction phase. Here a CHEAP Planning Writer drafts the FULL
        // detailed plan, and the Thinker merely READS the draft and emits a tiny
        // approve/revise decision — reading is far cheaper than writing, so the
        // costly long plan body stays on the cheap model. The whole loop runs in
        // a PRIVATE context (`writer_ctx`) so its turns never enter the shared
        // swarm history, keeping other turns' context cache small. Only the
        // final approved plan enters `shared`.
        let planning_writer_provider =
            resolve_role_provider(AgentRole::PlanningWriter, &app_data_dir).await?;
        let thinker_provider =
            resolve_role_provider(AgentRole::Thinker, &app_data_dir).await?;

        emit(AgentEventKind::PhaseStarted {
            role: AgentRole::PlanningWriter.key().to_string(),
            label: "Planning Writer: drafting detailed plan".to_string(),
            model: planning_writer_provider.name().to_string(),
        });

        // PRIVATE writer context: seeded with the full Thinker context/cache
        // (including all research, reasoning, and Thinker messages),
        // accumulates the Thinker's revision notes across rounds, but is NEVER
        // written back into `shared`.
        let mut writer_ctx: Vec<Message> = if !thinker_history.is_empty() {
            thinker_history.clone()
        } else {
            shared.clone()
        };
        let writer_spec = AgentRole::PlanningWriter.spec();
        let mut draft_plan: Option<SwarmPlan> = None;
        let mut review_rounds = 0usize;
        let mut last_revision_notes: Option<String> = None;
        let mut last_draft_md: Option<String> = None;
        let mut raw_draft_doc: Option<String> = None;
        // This whole phase (writer drafts + Thinker reviews) shares one Thinker
        // section in the UI, so aggregate its per-role totals for the badge.
        let mut planner_phase_tokens_in: usize = 0;
        let mut planner_phase_cached_in: usize = 0;
        let mut planner_phase_tokens_out: usize = 0;

        loop {
            let writer_outcome = self
                .inner
                .run_role_loop(
                    RoleLoopParams {
                        system_prompt: PromptComposer::compose_role_prompt(
                            AgentRole::PlanningWriter,
                            &project_root,
                        ),
                        messages: writer_ctx.clone(),
                        allowed_tools: &writer_spec.allowed_tools,
                        provider: planning_writer_provider.clone(),
                        max_turns: writer_spec.max_turns,
                        temperature: writer_spec.temperature,
                        stop_on_tool: Some("submit_plan"),
                        role_name: AgentRole::PlanningWriter.key(),
                    },
                    tool_ctx,
                    auto_approve,
                    &emit,
                )
                .await?;
            total_tokens += writer_outcome.tokens_used;
            total_tokens_in += writer_outcome.tokens_in;
            total_cached_in += writer_outcome.cached_in;
            total_tokens_out += writer_outcome.tokens_out;
            planner_phase_tokens_in += writer_outcome.tokens_in;
            planner_phase_cached_in += writer_outcome.cached_in;
            planner_phase_tokens_out += writer_outcome.tokens_out;
            writer_ctx = writer_outcome.history;

            let (raw_plan_doc, this_plan) = match &writer_outcome.stop_tool_args {
                Some(args) => match handoff_doc(&project_root, args, &writer_outcome.final_text) {
                    Ok(doc) => {
                        let plan = parse_plan_doc(&doc).ok();
                        (Some(doc), plan)
                    }
                    Err(_) => (None, None),
                },
                None => (None, None),
            };
            match &this_plan {
                Some(plan) => emit(AgentEventKind::PhaseCompleted {
                    role: AgentRole::PlanningWriter.key().to_string(),
                    summary: format!("Draft plan written: {} task(s)", plan.tasks.len()),
                    tokens_in: writer_outcome.tokens_in,
                    tokens_out: writer_outcome.tokens_out,
                    cached_in: writer_outcome.cached_in,
                }),
                None => {
                    emit(AgentEventKind::PhaseCompleted {
                        role: AgentRole::PlanningWriter.key().to_string(),
                        summary: if writer_outcome.exhausted_turns {
                            "Hit the turn limit without submitting a plan".to_string()
                        } else {
                            "Ended without a plan document".to_string()
                        },
                        tokens_in: writer_outcome.tokens_in,
                        tokens_out: writer_outcome.tokens_out,
                        cached_in: writer_outcome.cached_in,
                    });
                    if draft_plan.is_some() {
                        // A later revision round failed: keep the last good draft.
                        break;
                    }
                    return Err(AppError::General(if writer_outcome.exhausted_turns {
                        "Planning Writer hit the turn limit without submitting a plan".to_string()
                    } else {
                        "Planning Writer ended without a plan document".to_string()
                    }));
                }
            }
            if let Some(plan) = this_plan {
                let plan_path = project_root.join(".kuda").join("plan.md");
                let disk_raw = std::fs::read_to_string(&plan_path).ok().filter(|s| !s.trim().is_empty());
                let md = disk_raw.or(raw_plan_doc.clone()).unwrap_or_else(|| render_plan_markdown(&plan));
                if last_draft_md.as_deref() == Some(md.as_str()) {
                    // The rewrite left the plan identical — applying the notes
                    // made no difference, so further rounds would not converge.
                    tracing::info!(
                        "Planning Writer rewrite left the plan unchanged; accepting the draft"
                    );
                    break;
                }
                last_draft_md = Some(md.clone());
                raw_draft_doc = Some(md);
                draft_plan = Some(plan);
            }

            // ── Thinker review: READ the draft + emit a tiny decision ──────
            // PRIVATE review context (shared + the draft); never written back
            // into `shared`, so the review turns do not bloat later turns.
            let Some(ref current_plan) = draft_plan else {
                // Unreachable: a draft exists whenever we reach the review.
                break;
            };
            let plan_path = project_root.join(".kuda").join("plan.md");
            let disk_raw = std::fs::read_to_string(&plan_path).ok().filter(|s| !s.trim().is_empty());
            let plan_md_owned = disk_raw.or_else(|| raw_draft_doc.clone()).unwrap_or_else(|| render_plan_markdown(current_plan));
            let plan_md = plan_md_owned.as_str();
            emit(AgentEventKind::PhaseStarted {
                role: AgentRole::Thinker.key().to_string(),
                label: if review_rounds == 0 {
                    "Thinker: reviewing draft plan".to_string()
                } else {
                    format!("Thinker: reviewing draft plan (round {})", review_rounds + 1)
                },
                model: thinker_provider.name().to_string(),
            });
            let review_prompt = format!(
                "{}\n\nPLAN REVIEW MODE (STAGE B): The Planning Writer drafted the full plan \
                 below by expanding YOUR approved direction. You did NOT write it. READ it \
                 carefully and check it matches your intended design: the goal, the \
                 architecture, the chosen files, the task split, and the exact anchors/values.\n\n\
                 CRITICAL EFFICIENCY & DECISION RULES:\n\
                 1. Be DECISIVE, FOCUSED, and CONCISE. Do NOT over-analyze hypothetical internal crate \
                    mechanics or write long internal essays.\n\
                 2. Verify the core concrete checklist: files, cargo dependencies, endpoints, database \
                    schema/options, and error handling.\n\
                 3. If the plan matches your direction and is executable, APPROVE IT IMMEDIATELY without \
                    hesitation (set \"approved\": true).\n\
                 4. Only reject (set \"approved\": false) if there is a fatal compilation error or breaking \
                    logic gap. If rejecting, list all concrete bullet fixes concisely so the Planning \
                    Writer can apply all surgical fixes in ONE single pass.\n\
                 5. Call submit_plan_review exactly once in turn 1.",
                PromptComposer::compose_role_prompt(AgentRole::Thinker, &project_root)
            );
            let mut review_ctx: Vec<Message> = shared.clone();
            review_ctx.push(Message::user(format!(
                "[DRAFT PLAN written by the Planning Writer — review it against your approved \
                 direction and the brief]:\n{}",
                plan_md
            )));
            let review_outcome = self
                .inner
                .run_role_loop(
                    RoleLoopParams {
                        system_prompt: review_prompt,
                        messages: review_ctx,
                        allowed_tools: &["submit_plan_review".to_string()],
                        provider: thinker_provider.clone(),
                        max_turns: 2,
                        temperature: 0.2,
                        stop_on_tool: Some("submit_plan_review"),
                        role_name: "thinker (plan review)",
                    },
                    tool_ctx,
                    auto_approve,
                    &emit,
                )
                .await?;
            total_tokens += review_outcome.tokens_used;
            total_tokens_in += review_outcome.tokens_in;
            total_cached_in += review_outcome.cached_in;
            total_tokens_out += review_outcome.tokens_out;
            planner_phase_tokens_in += review_outcome.tokens_in;
            planner_phase_cached_in += review_outcome.cached_in;
            planner_phase_tokens_out += review_outcome.tokens_out;
            let (approved, revision_notes) = parse_plan_review(&review_outcome.stop_tool_args);
            emit(AgentEventKind::PhaseCompleted {
                role: AgentRole::Thinker.key().to_string(),
                summary: if approved {
                    "Draft plan approved".to_string()
                } else {
                    format!(
                        "Revision requested (round {}): {}",
                        review_rounds + 1,
                        truncate_chars(revision_notes.as_deref().unwrap_or("(no notes)"), 200)
                    )
                },
                tokens_in: review_outcome.tokens_in,
                tokens_out: review_outcome.tokens_out,
                cached_in: review_outcome.cached_in,
            });

            if approved {
                break;
            }
            let notes_str = revision_notes.clone().unwrap_or_default();
            if last_revision_notes.as_deref() == Some(notes_str.as_str()) {
                // The Thinker repeated the exact same revisions — no new
                // information, so further rounds would just oscillate. Accept
                // the latest draft.
                tracing::info!(
                    "Thinker plan review repeated identical revision notes; accepting the draft"
                );
                break;
            }
            last_revision_notes = Some(notes_str.clone());
            review_rounds += 1;
            if review_rounds >= PLAN_WRITER_SAFETY_CAP {
                tracing::warn!(
                    "Planning Writer review loop hit the {}-round safety cap; accepting last draft",
                    PLAN_WRITER_SAFETY_CAP
                );
                break;
            }
            // Feed the revision notes back into the writer's PRIVATE context.
            writer_ctx.push(Message::user(format!(
                "[THINKER REVISION REQUEST] The Thinker reviewed your plan and asks for these \
                 corrections. Apply these fixes by SURGICALLY EDITING \".kuda/plan.md\" using \
                 multi_replace_file (do not rewrite the whole file), then call submit_plan again:\n{}",
                revision_notes.unwrap_or_default()
            )));
            emit(AgentEventKind::PhaseStarted {
                role: AgentRole::PlanningWriter.key().to_string(),
                label: format!(
                    "Planning Writer: revising plan (round {})",
                    review_rounds + 1
                ),
                model: planning_writer_provider.name().to_string(),
            });
        }

        let draft_plan = draft_plan.ok_or_else(|| {
            AppError::General("Planning Writer produced no plan.".to_string())
        })?;

        // ── Reviewer utama (WAJIB) setelah plan selesai dibuat ────────────
        // Reviewer utama (Kimi-K3, read-only) mengaudit plan yang sudah jadi:
        // mencari bug / kesalahan logika / kekurangan & hal yang bisa
        // ditingkatkan. Arahannya dikirim ke Thinker, yang memutuskan revisi,
        // lalu Planning Writer menulis ulang. Berulang sampai Reviewer utama
        // menyetujui (cap lama dihapus; guard no-progress mencegah oscilasi).
        let reviewer_base_ctx = if !thinker_history.is_empty() {
            &thinker_history
        } else {
            &shared
        };
        let mut final_plan = self
            .run_reviewer_improvement_loop(
                reviewer_base_ctx,
                &draft_plan,
                &project_root,
                &app_data_dir,
                tool_ctx,
                auto_approve,
                &emit,
                &mut total_tokens,
                &mut total_tokens_in,
                &mut total_tokens_out,
                &mut total_cached_in,
            )
            .await?;

        // Only the final approved plan enters the shared context — the entire
        // writer/review loop stays private so it never bloats later turns.
        shared.push(Message::user(format!(
            "[PLAN] Final plan — drafted by the Planning Writer and audited by the \
             Reviewer utama:\n{}",
            truncate_chars(&render_plan_markdown(&final_plan), 8000)
        )));
        emit(AgentEventKind::PhaseCompleted {
            role: AgentRole::Thinker.key().to_string(),
            summary: format!("Draft plan: {} task(s)", final_plan.tasks.len()),
            tokens_in: planner_phase_tokens_in,
            tokens_out: planner_phase_tokens_out,
            cached_in: planner_phase_cached_in,
        });


        // ── Phase 2: Plan Approval Gate (same context) ─────────────────────
        // When the gate is enabled (default), the run pauses after the plan
        // (already audited by the Reviewer utama) and waits for the user:
        // "execute", "revise" (with a note), or "review" (re-run the Reviewer
        // utama improvement loop). Waiting costs 0 tokens and stops a
        // misunderstood plan BEFORE the expensive executor phase runs.
        // When the gate is off, the reviewer already ran automatically.
        let plan_status_str: Option<String>;
        let gate_enabled = cfg.agent.plan_gate_enabled;

        if gate_enabled {
            let mut gate_rounds = 0usize;
            let mut revisions = 0usize;
            let mut reviews = 0usize;
            let mut latest_note: Option<String> = None;
            let mut cancelled = false;
            loop {
                let request_id = format!("plan_{}", uuid::Uuid::new_v4().simple());
                let rx = tool_ctx.plan_decisions.register(&request_id);
                // `round` = number of prior gate presentations (0-based). The
                // UI disables revise/review once `round >= MAX_PLAN_GATE_ROUNDS`,
                // matching the backend's `can_modify` below.
                let round = gate_rounds;
                let plan_file_path = ".kuda/plan.md".to_string();
                let plan_path = project_root.join(&plan_file_path);
                let disk_raw = std::fs::read_to_string(&plan_path).ok().filter(|s| !s.trim().is_empty());
                let plan_md = disk_raw.unwrap_or_else(|| render_plan_markdown(&final_plan));
                // Persist the current plan to a reviewable artifact in the
                // project so the user can open it in the editor.
                if let Ok(canon) =
                    PathGuard::validate_path_in_scope(&plan_path, &project_root)
                {
                    if let Some(parent) = canon.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::write(&canon, &plan_md);
                }
                emit(AgentEventKind::PlanDecisionRequest {
                    request_id: request_id.clone(),
                    plan_markdown: plan_md,
                    plan_file_path: plan_file_path.clone(),
                    round,
                    tasks_count: final_plan.tasks.len(),
                    latest_note: latest_note.clone(),
                });
                let decision = tokio::select! {
                    r = rx => match r {
                        Ok(d) => d,
                        Err(_) => ("cancelled".to_string(), None),
                    },
                    _ = tool_ctx.cancel.notified() => ("cancelled".to_string(), None),
                };
                // Drop the stale registry entry on the cancel/timeout path.
                tool_ctx.plan_decisions.remove(&request_id);
                emit(AgentEventKind::PlanDecisionResolved {
                    request_id,
                    decision: decision.0.clone(),
                    note: decision.1.clone(),
                });
                gate_rounds += 1;
                // After the cap, revise/review are disabled: only an explicit
                // "execute" or a cancel may leave the gate.
                let can_modify = round < MAX_PLAN_GATE_ROUNDS;

                match decision.0.as_str() {
                    "execute" => {
                        // Execution ONLY happens on an explicit user click.
                        break;
                    }
                    "revise" if can_modify => {
                        revisions += 1;
                        latest_note = decision.1;
                        let note_text = latest_note.clone().unwrap_or_default();
                        let mut revise_ctx = shared.clone();
                        revise_ctx.push(Message::user(format!(
                            "[USER PLAN REVISION] The user wants these changes to the plan:\n{}\n\
                             Revise the plan accordingly using the same plan template, then call \
                             submit_plan exactly once with {{\"file_path\": \".kuda/plan.md\"}}.",
                            note_text
                        )));
                        let thinker_spec = AgentRole::Thinker.spec();
                        let revise_outcome = self
                            .inner
                            .run_role_loop(
                                RoleLoopParams {
                                    system_prompt: PromptComposer::compose_role_prompt(AgentRole::Thinker, &project_root),
                                    messages: revise_ctx,
                                    allowed_tools: &thinker_spec.allowed_tools,
                                    provider: thinker_provider.clone(),
                                    max_turns: thinker_spec.max_turns,
                                    temperature: thinker_spec.temperature,
                                    stop_on_tool: Some("submit_plan"),
                                    role_name: "thinker (plan revision)",
                                },
                                tool_ctx,
                                auto_approve,
                                &emit,
                            )
                            .await?;
                        total_tokens += revise_outcome.tokens_used;
                        total_tokens_in += revise_outcome.tokens_in;
                        total_cached_in += revise_outcome.cached_in;
                        total_tokens_out += revise_outcome.tokens_out;
                        if let Some(args) = &revise_outcome.stop_tool_args {
                            if let Ok(doc) = handoff_doc(&project_root, args, &revise_outcome.final_text) {
                                if let Ok(plan) = parse_plan_doc(&doc) {
                                    final_plan = plan;
                                }
                            }
                        }
                        // Keep `shared` consistent with the plan executors will
                        // actually run: the old `[PLAN]` was the pre-revision
                        // draft. Pushing a fresh `[PLAN]` (instead of replacing
                        // `shared` with the Thinker's private revision history,
                        // which used to leak the whole private loop into every
                        // later executor/reviewer turn) keeps one authoritative
                        // plan in context.
                        shared.push(Message::user(format!(
                            "[PLAN] Plan direvisi sesuai instruksi user:\n{}",
                            truncate_chars(&render_plan_markdown(&final_plan), 8000)
                        )));
                    }
                    "review" if can_modify => {
                        reviews += 1;
                        final_plan = self
                            .run_reviewer_improvement_loop(
                                &shared,
                                &final_plan,
                                &project_root,
                                &app_data_dir,
                                tool_ctx,
                                auto_approve,
                                &emit,
                                &mut total_tokens,
                                &mut total_tokens_in,
                                &mut total_tokens_out,
                                &mut total_cached_in,
                            )
                            .await?;
                        // Same consistency fix as "revise": the reviewed plan is
                        // what executors will run, so refresh the `[PLAN]` block.
                        shared.push(Message::user(format!(
                            "[PLAN] Plan direview & ditingkatkan Reviewer:\n{}",
                            truncate_chars(&render_plan_markdown(&final_plan), 8000)
                        )));
                    }
                    "cancelled" => {
                        cancelled = true;
                        break;
                    }
                    _ => {
                        // Unknown decision, or revise/review after the cap:
                        // NEVER auto-enter execution — go back to the gate and
                        // wait for an explicit execute or a cancel.
                        continue;
                    }
                }
            }

            if cancelled {
                // Graceful stop: persist the plan draft as a cancelled ledger so
                // the next turn knows a plan was left behind.
                let final_text = "(dibatalkan user pada tahap approval plan)".to_string();
                ledger.plan_markdown = Some(truncate_chars(&render_plan_markdown(&final_plan), LEDGER_PLAN_CHARS));
                ledger.plan_status = Some("CANCELLED AT GATE".to_string());
                ledger.final_answer = truncate_chars(&final_text, LEDGER_ANSWER_CHARS);
                emit(AgentEventKind::Finished {
                    total_tokens_used: total_tokens,
                    tokens_in: total_tokens_in,
                    tokens_out: total_tokens_out,
                    cached_in: total_cached_in,
                });
                return Ok(SwarmOutcome {
                    final_answer: final_text,
                    plan: Some(final_plan),
                    verdict: None,
                    tokens_used: total_tokens,
                    ledger,
                    transcript: transcript.lock().map(|mut c| c.finish()).unwrap_or_default(),
                });
            }

            plan_status_str = Some(if revisions == 0 && reviews == 0 {
                "approved (no changes)".to_string()
            } else {
                format!("approved (review×{}, revision×{})", reviews, revisions)
            });
        } else {
            // Gate off → the Reviewer utama already ran right after the plan was
            // created (mandatory), so nothing extra happens here.
            plan_status_str = Some("auto (gate off, reviewer utama sudah jalan)".to_string());
        }

        // Record the approved/revised plan + status into the turn's ledger.
        ledger.plan_markdown = Some(truncate_chars(&render_plan_markdown(&final_plan), LEDGER_PLAN_CHARS));
        ledger.plan_status = plan_status_str;

        // Carry the approved plan out of the gate block for the execution phase.
        approved_plan = Some(final_plan.clone());
        } // end `if !resume_executing` — planning + plan gate are skipped on an
          // Executing resume (the approved plan comes from the checkpoint).

        // ── Phase 3+4: Execute tasks, then verify (with one fix round) ─────
        let approved_plan = approved_plan.ok_or_else(|| {
            AppError::General("No approved plan available for execution.".to_string())
        })?;
        let mut tasks: Vec<SwarmPlanTask> = if resume_executing {
            resume
                .as_ref()
                .map(|r| r.pending_tasks.clone())
                .unwrap_or_default()
        } else {
            normalize_tasks(approved_plan.tasks.clone())
        };
        let mut fix_round: usize = if resume_executing {
            resume.as_ref().map(|r| r.fix_round).unwrap_or(0)
        } else {
            0
        };
        // Completed tasks' edit transcripts (Executor Reviewer needs them).
        let seed_executor_logs: Vec<Message> = if resume_executing {
            resume
                .as_ref()
                .map(|r| r.executor_logs.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // The seed applies to the FIRST execution round only; a fix round starts
        // its own fresh log accumulation.
        let mut seed_consumed = !resume_executing;
        let mut verdict_opt: Option<Verdict>;

        // Checkpoint once execution starts: a failure in any later task or the
        // verifier resumes from here (pending tasks only).
        if !resume_executing {
            write_checkpoint(
                &app_data_dir,
                &run_id,
                &RunCheckpoint {
                    phase: ResumePhase::Executing,
                    shared: shared.clone(),
                    total_tokens,
                    tokens_in: total_tokens_in,
                    tokens_out: total_tokens_out,
                    cached_in: total_cached_in,
                    ledger: ledger.clone(),
                    final_plan: Some(approved_plan.clone()),
                    pending_tasks: tasks.clone(),
                    executor_logs: Vec::new(),
                    fix_round: 0,
                    exec_notes: exec_notes.clone(),
                },
            );
        }

        loop {
            // Full transcripts of every executor's edit conversation in this round.
            // These are given to the Executor Reviewer (so it can confirm every part
            // was actually applied) but kept OUT of `shared` so the Thinker only sees diffs.
            let mut executor_logs: Vec<Message> = if !seed_consumed {
                seed_consumed = true;
                seed_executor_logs.clone()
            } else {
                Vec::new()
            };

            for (task_idx, task) in tasks.iter().enumerate() {
                let role = if task.kind.eq_ignore_ascii_case("design") {
                    AgentRole::ExecutorDesign
                } else {
                    AgentRole::ExecutorCode
                };
                let executor_provider = resolve_role_provider(role, &app_data_dir).await?;
                emit(AgentEventKind::PhaseStarted {
                    role: role.key().to_string(),
                    label: format!("{}: task #{} ({})", role.display_name(), task.id, task.kind),
                    model: executor_provider.name().to_string(),
                });

                // Executor gets the full Thinker context cache (including Thinker reasoning)
                // + approved plan & shared task progress.
                let exec_start = shared.len();
                let mut exec_history = if !thinker_history.is_empty() {
                    let mut h = thinker_history.clone();
                    for msg in shared.iter().skip(thinker_history.len().min(shared.len())) {
                        h.push(msg.clone());
                    }
                    h
                } else {
                    shared.clone()
                };
                exec_history.push(Message::user(build_executor_brief(&approved_plan, task)));

                // Snapshot the WHOLE project tree BEFORE execution so the diff
                // report catches every change the executor makes — including
                // edits performed through `run_command`, which bypass checkpoints.
                let tree_before = snapshot_project_tree(tool_ctx);

                let exec_spec = role.spec();
                let exec_outcome = self
                    .inner
                    .run_role_loop(
                        RoleLoopParams {
                            system_prompt: PromptComposer::compose_role_prompt(role, &project_root),
                            messages: exec_history,
                            allowed_tools: &exec_spec.allowed_tools,
                            provider: executor_provider,
                            max_turns: exec_spec.max_turns,
                            temperature: exec_spec.temperature,
                            stop_on_tool: None,
                            role_name: role.key(),
                        },
                        tool_ctx,
                        auto_approve,
                        &emit,
                    )
                    .await?;
                total_tokens += exec_outcome.tokens_used;
                total_tokens_in += exec_outcome.tokens_in;
                total_cached_in += exec_outcome.cached_in;
                total_tokens_out += exec_outcome.tokens_out;

                // Capture the executor's own added messages (task brief + its tool turns
                // and results) for the Executor Reviewer. The shared prefix is not duplicated.
                executor_logs.extend(exec_outcome.history[exec_start..].to_vec());

                // Only the DIFF is appended back to shared context — the Thinker
                // never sees the executor's internal edit conversation.
                let diff_report = build_tree_diff_report(tool_ctx, &tree_before);
                let mut exec_summary = diff_report.lines().next().unwrap_or("no changes").to_string();
                if exec_outcome.exhausted_turns {
                    exec_summary.push_str(" (executor hit its turn limit — output may be incomplete)");
                }
                // Condensed one-liner for the turn's ledger (display of the
                // same information, far smaller than the full report).
                let diff_head = diff_report
                    .lines()
                    .next()
                    .unwrap_or("no changes")
                    .to_string();
                exec_notes.push(format!(
                    "[EXEC] Task #{} ({}) by {} — {}",
                    task.id,
                    task.kind,
                    role.display_name(),
                    truncate_chars(&diff_head, 300)
                ));
                let report_text = format!(
                    "[EXECUTOR REPORT] Task #{} ({}) finished by {}.\n{}",
                    task.id,
                    task.kind,
                    role.display_name(),
                    truncate_chars(&diff_report, MAX_REPORT_CHARS)
                );
                shared.push(Message::user(report_text.clone()));
                emit(AgentEventKind::PhaseCompleted {
                    role: role.key().to_string(),
                    summary: exec_summary,
                    tokens_in: exec_outcome.tokens_in,
                    tokens_out: exec_outcome.tokens_out,
                    cached_in: exec_outcome.cached_in,
                });

                // Refresh the resume checkpoint after each completed task so a
                // failure in a later task / the verifier resumes from the NEXT
                // task (completed edits are never re-applied).
                let mut remaining = tasks.clone();
                if task_idx < remaining.len() {
                    remaining.drain(..task_idx + 1);
                }
                write_checkpoint(
                    &app_data_dir,
                    &run_id,
                    &RunCheckpoint {
                        phase: ResumePhase::Executing,
                        shared: shared.clone(),
                        total_tokens,
                        tokens_in: total_tokens_in,
                        tokens_out: total_tokens_out,
                        cached_in: total_cached_in,
                        ledger: ledger.clone(),
                        final_plan: Some(approved_plan.clone()),
                        pending_tasks: remaining,
                        executor_logs: executor_logs.clone(),
                        fix_round,
                        exec_notes: exec_notes.clone(),
                    },
                );
            }

            // ── Phase 4: Executor Reviewer verifies ─────────────────────────
            let verifier_provider = resolve_role_provider(AgentRole::ExecutorReviewer, &app_data_dir).await?;
            emit(AgentEventKind::PhaseStarted {
                role: AgentRole::ExecutorReviewer.key().to_string(),
                label: "Executor Reviewer: verifying results".to_string(),
                model: verifier_provider.name().to_string(),
            });

            // The Executor Reviewer must confirm every part was actually applied, so it
            // receives the full executor transcripts (not just the diffs the Thinker sees).
            let mut verify_context = shared.clone();
            if !executor_logs.is_empty() {
                verify_context.push(Message::user(
                    "[EXECUTOR CONTEXT] Below are the executors' full edit transcripts \
                     (task briefs, tool calls and results). Use them together with the \
                     diff reports above to verify completeness.",
                ));
                verify_context.extend(executor_logs);
            }
            verify_context.push(Message::user(
                "[SWARM] Verification step: verify every plan task against the executor \
                 work above (read files / run checks as needed), then submit your final \
                 verdict via submit_verdict.",
            ));
            let verifier_spec = AgentRole::ExecutorReviewer.spec();
            let verifier_outcome = self
                .inner
                .run_role_loop(
                    RoleLoopParams {
                        system_prompt: PromptComposer::compose_role_prompt(AgentRole::ExecutorReviewer, &project_root),
                        messages: verify_context,
                        allowed_tools: &verifier_spec.allowed_tools,
                        provider: verifier_provider,
                        max_turns: verifier_spec.max_turns,
                        temperature: verifier_spec.temperature,
                        stop_on_tool: Some("submit_verdict"),
                        role_name: AgentRole::ExecutorReviewer.key(),
                    },
                    tool_ctx,
                    auto_approve,
                    &emit,
                )
                .await?;
            total_tokens += verifier_outcome.tokens_used;
            total_tokens_in += verifier_outcome.tokens_in;
            total_cached_in += verifier_outcome.cached_in;
            total_tokens_out += verifier_outcome.tokens_out;

            // A missing or unparseable verdict means the results could NOT be
            // confirmed — it carries no actionable issue list, so it must never
            // spawn a fix round of fabricated tasks. Only a genuinely parsed
            // verdict (passed or failed with real issues) is confirmable.
            let (verdict, confirmable) = match &verifier_outcome.stop_tool_args {
                Some(args) => match handoff_doc(&project_root, args, &verifier_outcome.final_text) {
                    Ok(doc) => match parse_verdict_doc(&doc) {
                        Ok(v) => (v, true),
                        Err(_) => (
                            Verdict {
                                passed: false,
                                summary: "Executor Reviewer verdict document was unparseable; results could not be confirmed.".to_string(),
                                issues: vec![],
                            },
                            false,
                        ),
                    },
                    Err(_) => (
                        Verdict {
                            passed: false,
                            summary: "Executor Reviewer did not produce a verdict document; results could not be confirmed.".to_string(),
                            issues: vec![],
                        },
                        false,
                    ),
                },
                None => (
                    Verdict {
                        passed: false,
                        summary: if verifier_outcome.exhausted_turns {
                            "Executor Reviewer hit its turn limit without submitting a verdict; results could not be confirmed.".to_string()
                        } else {
                            "Executor Reviewer did not submit a verdict; results could not be confirmed.".to_string()
                        },
                        issues: vec![],
                    },
                    false,
                ),
            };
            emit(AgentEventKind::PhaseCompleted {
                role: AgentRole::ExecutorReviewer.key().to_string(),
                summary: if confirmable {
                    format!(
                        "{} — {}",
                        if verdict.passed { "PASSED" } else { "FAILED" },
                        truncate_chars(&verdict.summary, 200)
                    )
                } else {
                    format!("UNVERIFIED — {}", truncate_chars(&verdict.summary, 200))
                },
                tokens_in: verifier_outcome.tokens_in,
                tokens_out: verifier_outcome.tokens_out,
                cached_in: verifier_outcome.cached_in,
            });

            // Only the short verdict lands back in the shared context (so the Thinker knows
            // the outcome without exposing the executors' internal transcripts).
            shared.push(Message::user(format!("[SWARM] Verification result: {}", verdict.summary)));

            if !confirmable {
                // No actionable issues: end the round as unverified instead of
                // asking an executor to "fix" a fabricated task.
                verdict_opt = None;
                break;
            }

            let needs_fix = !verdict.passed && !verdict.issues.is_empty() && fix_round < MAX_FIX_ROUNDS;
            verdict_opt = Some(verdict.clone());
            if !needs_fix {
                break;
            }

            // Build fix tasks from verified issues and run one more execution round.
            fix_round += 1;
            let base_id = tasks.iter().map(|t| t.id).max().unwrap_or(0);
            shared.push(Message::user(format!(
                "[SWARM] Verification failed (fix round {}). Fix tasks created from issues.",
                fix_round
            )));
            tasks = normalize_tasks(
                verdict
                    .issues
                    .iter()
                    .enumerate()
                    .map(|(i, issue)| SwarmPlanTask {
                        id: base_id + 1 + i as u32,
                        kind: if issue.kind.eq_ignore_ascii_case("design") { "design".into() } else { "code".into() },
                        description: format!("FIX from verification: {}", issue.description),
                        context: None,
                        files: vec![],
                        acceptance: format!("Resolve: {}", issue.description),
                    })
                    .collect(),
            );
        }

        // ── Phase 5: Thinker final answer from the shared context ──────────
        let final_provider = resolve_role_provider(AgentRole::Thinker, &app_data_dir).await?;
        emit(AgentEventKind::PhaseStarted {
            role: AgentRole::Thinker.key().to_string(),
            label: "Thinker: final answer".to_string(),
            model: final_provider.name().to_string(),
        });
        let verdict_state = match &verdict_opt {
            Some(v) if v.passed => "All tasks finished and verified by the Executor Reviewer.".to_string(),
            Some(v) => format!(
                "Verification finished with {} unresolved issue(s): {}",
                v.issues.len(),
                truncate_chars(&v.summary, 300)
            ),
            None => "Execution finished (no verification verdict was produced).".to_string(),
        };
        shared.push(Message::user(format!(
            "[SWARM] {}\nWrite the final answer for the user: summarize what changed and why, \
             based on the executor reports above. Be honest about anything still unresolved \
             or unverified. Be concise and structured.",
            verdict_state
        )));
        let final_outcome = self
            .inner
            .run_role_loop(
                RoleLoopParams {
                    system_prompt: PromptComposer::compose_role_prompt(AgentRole::Thinker, &project_root),
                    messages: shared,
                    allowed_tools: &[],
                    provider: final_provider,
                    max_turns: 1,
                    temperature: 0.2,
                    stop_on_tool: None,
                    role_name: AgentRole::Thinker.key(),
                },
                tool_ctx,
                auto_approve,
                &emit,
            )
            .await?;
        total_tokens += final_outcome.tokens_used;
        total_tokens_in += final_outcome.tokens_in;
        total_cached_in += final_outcome.cached_in;
        total_tokens_out += final_outcome.tokens_out;
        emit(AgentEventKind::PhaseCompleted {
            role: AgentRole::Thinker.key().to_string(),
            summary: "Final answer delivered".to_string(),
            tokens_in: final_outcome.tokens_in,
            tokens_out: final_outcome.tokens_out,
            cached_in: final_outcome.cached_in,
        });

        emit(AgentEventKind::Finished {
            total_tokens_used: total_tokens,
            tokens_in: total_tokens_in,
            tokens_out: total_tokens_out,
            cached_in: total_cached_in,
        });

        ledger.execution_review = Some(truncate_chars(
            &build_execution_review_ledger(&exec_notes, &verdict_opt, &verdict_state),
            LEDGER_EXEC_CHARS,
        ));
        ledger.final_answer = truncate_chars(&final_outcome.final_text, LEDGER_ANSWER_CHARS);

        Ok(SwarmOutcome {
            final_answer: final_outcome.final_text,
            plan: Some(approved_plan),
            verdict: verdict_opt,
            tokens_used: total_tokens,
            ledger,
            transcript: transcript.lock().map(|mut c| c.finish()).unwrap_or_default(),
        })
    }

    /// Runs a single no-tool Thinker turn that writes the best possible final
    /// answer from the shared context. Used when planning fails or exhausts its
    /// turns, so the swarm degrades gracefully instead of aborting the run.
    async fn synthesize_answer_from_context(
        &self,
        shared: Vec<Message>,
        project_root: &Path,
        provider: Arc<dyn LlmProvider>,
        tool_ctx: &ToolContext,
        auto_approve: bool,
        emit: &impl Fn(AgentEventKind),
    ) -> Result<RoleLoopOutcome> {
        let mut final_ctx = shared;
        final_ctx.push(Message::user(
            "[SWARM] The planner could not produce an actionable execution plan. Write the best \
             final answer you can for the user based on the brief and the analysis above. Be \
             honest that no concrete execution plan was produced.",
        ));
        self.inner
            .run_role_loop(
                RoleLoopParams {
                    system_prompt: PromptComposer::compose_role_prompt(AgentRole::Thinker, project_root),
                    messages: final_ctx,
                    allowed_tools: &[],
                    provider,
                    max_turns: 1,
                    temperature: 0.2,
                    stop_on_tool: None,
                    role_name: AgentRole::Thinker.key(),
                },
                tool_ctx,
                auto_approve,
                emit,
            )
            .await
    }

    /// Bounded RLM supplement round: the Thinker asked the RLM researcher for
    /// additional data (`request_rlm_research`). Seeds the RLM loop with the
    /// shared context + the request, runs a bounded collection pass, and
    /// returns a compact (truncated) supplement plus its token usage.
    async fn run_rlm_research_round(
        &self,
        shared: &[Message],
        request_args: &str,
        tool_ctx: &ToolContext,
        auto_approve: bool,
        emit: &impl Fn(AgentEventKind),
    ) -> Result<(String, usize, usize, usize)> {
        let project_root = tool_ctx.project_root.clone();
        let app_data_dir = tool_ctx.app_data_dir.clone();

        let mut rlm_ctx: Vec<Message> = shared.to_vec();
        rlm_ctx.push(Message::user(format!(
            "[RESEARCH REQUEST — dikirim Thinker]\n{}\n\n\
             TARGETED SUPPLEMENT ROUND: the Thinker needs ADDITIONAL facts that the validated \
             brief does not contain. Collect ONLY the data points requested above; reuse the \
             kernel's memoized reads where possible (do NOT redo the research already summarized \
             in the brief). Write the supplement as markdown to the project file \
             \".kuda/supplement.md\" using write_file (create the .kuda/ directory if needed), \
             then call submit_brief exactly once with {{\"file_path\": \".kuda/supplement.md\"}}. \
             Keep it compact: answer the questions with verbatim evidence only.",
            request_args
        )));

        let model_provider = resolve_role_provider(AgentRole::RlmModel, &app_data_dir).await?;
        let model_spec = AgentRole::RlmModel.spec();
        emit(AgentEventKind::PhaseStarted {
            role: AgentRole::RlmModel.key().to_string(),
            label: "RLM Model: riset tambahan (dari Thinker)".to_string(),
            model: model_provider.name().to_string(),
        });

        let system_prompt = format!(
            "{}\n\nSUPPLEMENT ROUND: you are collecting a TARGETED supplement for the Thinker, \
             not a fresh full brief. The request above defines exactly what to find. Write the \
             supplement markdown to \".kuda/supplement.md\" (NOT \".kuda/brief.md\") and call \
             submit_brief with that path.",
            PromptComposer::compose_role_prompt(AgentRole::RlmModel, &project_root)
        );

        let outcome = self
            .inner
            .run_role_loop(
                RoleLoopParams {
                    system_prompt,
                    messages: rlm_ctx,
                    allowed_tools: &model_spec.allowed_tools,
                    provider: model_provider.clone(),
                    max_turns: RLM_SUPPLEMENT_MAX_TURNS,
                    temperature: model_spec.temperature,
                    stop_on_tool: Some("submit_brief"),
                    role_name: AgentRole::RlmModel.key(),
                },
                tool_ctx,
                auto_approve,
                emit,
            )
            .await?;

        let supp_path = project_root.join(".kuda").join("supplement.md");
        let raw_disk_supp = std::fs::read_to_string(&supp_path).ok().filter(|s| !s.trim().is_empty());
        let supplement = match &outcome.stop_tool_args {
            Some(args) => match handoff_doc(&project_root, args, &outcome.final_text) {
                Ok(doc) => {
                    let doc = resolve_snippet_placeholders(&project_root, &doc).await;
                    if let Ok(brief) = parse_brief_doc(&doc) {
                        if !brief.relevant_snippets.is_empty() || !brief.key_files.is_empty() {
                            format_brief_digest(
                                &brief,
                                &ContextAudit {
                                    complete: false,
                                    summary: "Supplement dikumpulkan RLM atas permintaan Thinker".to_string(),
                                    missing: vec![],
                                },
                            )
                        } else {
                            raw_disk_supp.unwrap_or(doc)
                        }
                    } else {
                        raw_disk_supp.unwrap_or(doc)
                    }
                }
                Err(_) => raw_disk_supp.unwrap_or_else(|| outcome.final_text.clone()),
            },
            None => {
                raw_disk_supp.unwrap_or_else(|| outcome.final_text.clone())
            }
        };

        emit(AgentEventKind::PhaseCompleted {
            role: AgentRole::RlmModel.key().to_string(),
            summary: "Riset tambahan selesai — supplement dikirim ke Thinker".to_string(),
            tokens_in: outcome.tokens_in,
            tokens_out: outcome.tokens_out,
            cached_in: outcome.cached_in,
        });

        Ok((
            supplement,
            outcome.tokens_in,
            outcome.tokens_out,
            outcome.cached_in,
        ))
    }

    /// Mandatory Reviewer-utama improvement loop, run right after the plan is
    /// finished. The Reviewer (smart model, read-only) audits the plan for bugs /
    /// logic errors / missing depth and returns DIRECTIONS via
    /// `submit_review_directions` (never writes files). The Thinker evaluates
    /// each direction and turns it into concrete revision notes, then the
    /// Planning Writer rewrites the plan. Loops (bounded) until the Reviewer
    /// approves.
    #[allow(clippy::too_many_arguments)]
    async fn run_reviewer_improvement_loop(
        &self,
        shared: &[Message],
        plan: &SwarmPlan,
        project_root: &Path,
        app_data_dir: &Path,
        tool_ctx: &ToolContext,
        auto_approve: bool,
        emit: &impl Fn(AgentEventKind),
        total_tokens: &mut usize,
        total_tokens_in: &mut usize,
        total_tokens_out: &mut usize,
        total_cached_in: &mut usize,
    ) -> Result<SwarmPlan> {
        let reviewer_provider = resolve_role_provider(AgentRole::Reviewer, app_data_dir).await?;
        let thinker_provider = resolve_role_provider(AgentRole::Thinker, app_data_dir).await?;
        let writer_provider =
            resolve_role_provider(AgentRole::PlanningWriter, app_data_dir).await?;
        let reviewer_spec = AgentRole::Reviewer.spec();
        let thinker_spec = AgentRole::Thinker.spec();
        let writer_spec = AgentRole::PlanningWriter.spec();

        let mut final_plan = plan.clone();
        let mut reviewed = false;
        let mut last_directions: Option<String> = None;
        let mut last_plan_md: Option<String> = None;
        let mut round = 0usize;

        loop {
            if round >= PLAN_IMPROVE_SAFETY_CAP {
                tracing::warn!(
                    "Reviewer-utama loop hit the {}-round safety cap; accepting the plan as-is",
                    PLAN_IMPROVE_SAFETY_CAP
                );
                break;
            }
            // ── Reviewer utama: audit the finished plan (read-only) ───────
            let plan_path = project_root.join(".kuda").join("plan.md");
            let disk_raw = std::fs::read_to_string(&plan_path).ok().filter(|s| !s.trim().is_empty());
            let plan_md = disk_raw.unwrap_or_else(|| render_plan_markdown(&final_plan));
            if last_plan_md.as_deref() == Some(plan_md.as_str()) {
                // The rewrite did not change the plan since the last audit —
                // the directions were not applied, so no further progress is
                // possible. Accept the plan as-is.
                tracing::info!(
                    "Reviewer-utama saw an unchanged plan after a rewrite; accepting it"
                );
                break;
            }
            last_plan_md = Some(plan_md.clone());
            emit(AgentEventKind::PhaseStarted {
                role: AgentRole::Reviewer.key().to_string(),
                label: if round == 0 {
                    "Reviewer utama: audit plan".to_string()
                } else {
                    format!("Reviewer utama: audit plan (round {})", round + 1)
                },
                model: reviewer_provider.name().to_string(),
            });
            let mut review_ctx: Vec<Message> = shared.to_vec();
            review_ctx.push(Message::user(format!(
                "[PLAN — mohon diaudit oleh Reviewer utama]\n{}",
                plan_md
            )));
            let reviewer_prompt = format!(
                "{}\n\nAUDIT MODE (Reviewer utama): you are auditing the COMPLETED plan. \
                 Periksa apa saja hal yang bisa ditingkatkan agar plan ini lebih baik, dan \
                 cari bug / kesalahan logika / rencana yang keliru atau tidak lengkap di \
                 plan ini — tujuannya MENINGKATKAN KUALITAS plan agar hasil eksekusinya lebih \
                 mendetail dan kompleks (arsitektur, alur data, error handling, edge cases, \
                 urutan task, ketergantungan antar task, nilai/identifer yang harus presisi).\n\n\
                 CRITICAL AUDIT INSTRUCTION: Conduct an EXHAUSTIVE, ALL-IN-ONE review of the \
                 ENTIRE plan in a single pass. Do NOT trickle feedback across multiple rounds. \
                 Inspect the entire architecture, crate dependencies, SQL options, Tokio async \
                 gotchas, handler error mapping, and JS logic. List ALL issues and concrete \
                 improvements in 'directions' with exact task/section names and required fixes.\n\n\
                 Anda READ-ONLY: JANGAN menulis file apa pun dan JANGAN menulis ulang plan. \
                 Panggil submit_review_directions tepat satu kali: \"approved\"=true bila plan \
                 sudah solid, atau \"approved\"=false dengan setiap perbaikan sebagai satu item \
                 di \"directions\" (sebutkan task/section persis, apa yang salah, dan apa yang \
                 harus diubah).",
                PromptComposer::compose_role_prompt(AgentRole::Reviewer, project_root)
            );
            let review_outcome = self
                .inner
                .run_role_loop(
                    RoleLoopParams {
                        system_prompt: reviewer_prompt,
                        messages: review_ctx,
                        allowed_tools: &reviewer_spec.allowed_tools,
                        provider: reviewer_provider.clone(),
                        max_turns: reviewer_spec.max_turns,
                        temperature: reviewer_spec.temperature,
                        stop_on_tool: Some("submit_review_directions"),
                        role_name: AgentRole::Reviewer.key(),
                    },
                    tool_ctx,
                    auto_approve,
                    emit,
                )
                .await?;
            *total_tokens += review_outcome.tokens_used;
            *total_tokens_in += review_outcome.tokens_in;
            *total_cached_in += review_outcome.cached_in;
            *total_tokens_out += review_outcome.tokens_out;
            let (approved, directions) = parse_review_directions(&review_outcome.stop_tool_args);
            emit(AgentEventKind::PhaseCompleted {
                role: AgentRole::Reviewer.key().to_string(),
                summary: if approved {
                    "Plan disetujui Reviewer utama".to_string()
                } else {
                    format!("Reviewer utama: {} arahan perbaikan", directions.len())
                },
                tokens_in: review_outcome.tokens_in,
                tokens_out: review_outcome.tokens_out,
                cached_in: review_outcome.cached_in,
            });
            if approved || directions.is_empty() {
                break;
            }

            // No-progress guard: identical directions as the previous round
            // mean the loop would just oscillate — accept the current plan.
            let dirs_key = directions.join("\n");
            if last_directions.as_deref() == Some(dirs_key.as_str()) {
                tracing::info!(
                    "Reviewer-utama repeated identical directions; accepting the current plan"
                );
                break;
            }
            last_directions = Some(dirs_key);

            // ── Thinker: evaluate the reviewer's directions → revision notes ──
            emit(AgentEventKind::PhaseStarted {
                role: AgentRole::Thinker.key().to_string(),
                label: format!("Thinker: evaluasi arahan Reviewer (round {})", round + 1),
                model: thinker_provider.name().to_string(),
            });
            let mut thinker_ctx: Vec<Message> = shared.to_vec();
            thinker_ctx.push(Message::user(format!(
                "[PLAN SAAT INI]\n{}\n\n[ARAHAN REVIEWER UTAMA]\n{}\nEvaluasi SETIAP arahan: \
                 mana yang benar dan layak diterapkan, mana yang Anda tolak beserta alasan. \
                 JANGAN menulis ulang plan. Panggil submit_plan_review: \"approved\"=true bila \
                 tidak ada yang perlu diubah, ATAU \"approved\"=false dengan catatan revisi \
                 konkret per item (nama task/section, apa yang salah, apa yang harus diubah) \
                 untuk Planning Writer.",
                plan_md,
                directions
                    .iter()
                    .map(|d| format!("- {}", d))
                    .collect::<Vec<_>>()
                    .join("\n")
            )));
            let thinker_eval_outcome = self
                .inner
                .run_role_loop(
                    RoleLoopParams {
                        system_prompt: format!(
                            "{}\n\nREVIEWER-FEEDBACK EVALUATION: decide which reviewer \
                             directions to accept and turn them into concrete revision notes. \
                             You do NOT write the plan — the Planning Writer does.",
                            PromptComposer::compose_role_prompt(AgentRole::Thinker, project_root)
                        ),
                        messages: thinker_ctx,
                        allowed_tools: &["submit_plan_review".to_string()],
                        provider: thinker_provider.clone(),
                        max_turns: 2,
                        temperature: thinker_spec.temperature,
                        stop_on_tool: Some("submit_plan_review"),
                        role_name: "thinker (evaluasi arahan reviewer)",
                    },
                    tool_ctx,
                    auto_approve,
                    emit,
                )
                .await?;
            *total_tokens += thinker_eval_outcome.tokens_used;
            *total_tokens_in += thinker_eval_outcome.tokens_in;
            *total_cached_in += thinker_eval_outcome.cached_in;
            *total_tokens_out += thinker_eval_outcome.tokens_out;
            let (thinker_approved, notes) = parse_plan_review(&thinker_eval_outcome.stop_tool_args);
            emit(AgentEventKind::PhaseCompleted {
                role: AgentRole::Thinker.key().to_string(),
                summary: if thinker_approved {
                    "Thinker menilai arahan Reviewer tidak perlu diterapkan".to_string()
                } else {
                    "Thinker menyetujui revisi — dikirim ke Planning Writer".to_string()
                },
                tokens_in: thinker_eval_outcome.tokens_in,
                tokens_out: thinker_eval_outcome.tokens_out,
                cached_in: thinker_eval_outcome.cached_in,
            });
            if thinker_approved || notes.is_none() {
                break;
            }

            // ── Planning Writer: rewrite the plan per the Thinker's notes ──
            let notes = notes.unwrap_or_default();
            let mut writer_ctx: Vec<Message> = shared.to_vec();
            writer_ctx.push(Message::user(format!(
                "[PLAN SAAT INI]\n{}",
                plan_md
            )));
            writer_ctx.push(Message::user(format!(
                "[THINKER REVISION REQUEST — berdasarkan arahan Reviewer utama] Terapkan \
                 koreksi berikut dengan mengedit .kuda/plan.md secara SURGICAL menggunakan \
                 multi_replace_file (jangan tulis ulang seluruh file), lalu panggil \
                 submit_plan dengan {{\"file_path\": \".kuda/plan.md\"}}:\n{}",
                notes
            )));
            emit(AgentEventKind::PhaseStarted {
                role: AgentRole::PlanningWriter.key().to_string(),
                label: format!("Planning Writer: menulis ulang plan (round {})", round + 1),
                model: writer_provider.name().to_string(),
            });
            let writer_outcome = self
                .inner
                .run_role_loop(
                    RoleLoopParams {
                        system_prompt:
                            PromptComposer::compose_role_prompt(AgentRole::PlanningWriter, project_root),
                        messages: writer_ctx,
                        allowed_tools: &writer_spec.allowed_tools,
                        provider: writer_provider.clone(),
                        max_turns: writer_spec.max_turns,
                        temperature: writer_spec.temperature,
                        stop_on_tool: Some("submit_plan"),
                        role_name: AgentRole::PlanningWriter.key(),
                    },
                    tool_ctx,
                    auto_approve,
                    emit,
                )
                .await?;
            *total_tokens += writer_outcome.tokens_used;
            *total_tokens_in += writer_outcome.tokens_in;
            *total_cached_in += writer_outcome.cached_in;
            *total_tokens_out += writer_outcome.tokens_out;
            let mut rewrote = false;
            if let Some(ref args) = writer_outcome.stop_tool_args {
                if let Ok(doc) = handoff_doc(project_root, args, &writer_outcome.final_text) {
                    if let Ok(p) = parse_plan_doc(&doc) {
                        final_plan = p;
                        rewrote = true;
                    }
                }
            }
            emit(AgentEventKind::PhaseCompleted {
                role: AgentRole::PlanningWriter.key().to_string(),
                summary: if rewrote {
                    format!("Plan ditulis ulang: {} task(s)", final_plan.tasks.len())
                } else {
                    "Gagal menulis ulang — mempertahankan plan sebelumnya".to_string()
                },
                tokens_in: writer_outcome.tokens_in,
                tokens_out: writer_outcome.tokens_out,
                cached_in: writer_outcome.cached_in,
            });
            if !rewrote {
                break;
            }
            reviewed = true;
            round += 1;
        }

        if !reviewed {
            tracing::info!("Reviewer utama menyetujui plan tanpa revisi.");
        }
        Ok(final_plan)
    }
}

fn parse_plan(args_json: &str) -> Result<SwarmPlan> {
    let v: serde_json::Value = serde_json::from_str(args_json)
        .map_err(|e| AppError::General(format!("Plan args are not valid JSON: {}", e)))?;
    let plan_v = v.get("plan").cloned().unwrap_or(v);
    let mut plan: SwarmPlan = serde_json::from_value(plan_v)
        .map_err(|e| AppError::General(format!("Invalid plan JSON: {}", e)))?;
    if plan.tasks.is_empty() {
        return Err(AppError::General("Plan contains no tasks".to_string()));
    }
    plan.tasks = normalize_tasks(plan.tasks);
    Ok(plan)
}

fn parse_verdict(args_json: &str) -> Result<Verdict> {
    let v: serde_json::Value = serde_json::from_str(args_json)
        .map_err(|e| AppError::General(format!("Verdict args are not valid JSON: {}", e)))?;
    let verdict_v = v.get("verdict").cloned().unwrap_or(v);
    serde_json::from_value(verdict_v)
        .map_err(|e| AppError::General(format!("Invalid verdict JSON: {}", e)))
}

fn parse_audit(args_json: &str) -> Result<ContextAudit> {
    let v: serde_json::Value = serde_json::from_str(args_json)
        .map_err(|e| AppError::General(format!("Audit args are not valid JSON: {}", e)))?;
    let audit_v = v.get("audit").cloned().unwrap_or(v);
    serde_json::from_value(audit_v)
        .map_err(|e| AppError::General(format!("Invalid audit JSON: {}", e)))
}

fn parse_brief(args_json: &str) -> Result<ResearchBrief> {
    let v: serde_json::Value = serde_json::from_str(args_json)
        .map_err(|e| AppError::General(format!("Brief args are not valid JSON: {}", e)))?;
    let brief_v = v.get("brief").cloned().unwrap_or(v);
    serde_json::from_value(brief_v)
        .map_err(|e| AppError::General(format!("Invalid brief JSON: {}", e)))
}

/// Parses the Thinker's `submit_plan_review` handoff args into
/// `(approved, revision_notes)`. The Thinker runs in PLAN REVIEW MODE inside
/// the Planning Writer loop: it READS the draft and emits only this tiny
/// decision (never rewrites the plan). Defaults to `(true, None)` when the
/// args are missing, malformed, or reject without actionable notes, so a
/// non-decision never loops forever — the plan gate still lets the user
/// revise afterward.
fn parse_plan_review(args_json: &Option<String>) -> (bool, Option<String>) {
    let Some(args) = args_json else {
        return (true, None);
    };
    let v: serde_json::Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(_) => return (true, None),
    };
    let approved = v.get("approved").and_then(|x| x.as_bool()).unwrap_or(true);
    if approved {
        return (true, None);
    }
    let notes = v
        .get("revision_notes")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if notes.is_empty() {
        // A rejection with no actionable notes cannot be applied — accept the
        // draft so the loop terminates; the user still revises at the gate.
        (true, None)
    } else {
        (false, Some(notes))
    }
}

/// Parses the Reviewer utama's `submit_review_directions` handoff args into
/// `(approved, directions)`. The Reviewer audits the finished plan (read-only)
/// and either approves it or returns one direction per required change.
/// Defaults to `(true, vec![])` when the args are missing/malformed, so a
/// non-decision never loops forever.
fn parse_review_directions(args_json: &Option<String>) -> (bool, Vec<String>) {
    let Some(args) = args_json else {
        return (true, Vec::new());
    };
    let v: serde_json::Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(_) => return (true, Vec::new()),
    };
    let approved = v.get("approved").and_then(|x| x.as_bool()).unwrap_or(true);
    if approved {
        return (true, Vec::new());
    }
    let directions: Vec<String> = v
        .get("directions")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|d| d.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if directions.is_empty() {
        // Rejection without actionable directions cannot be applied.
        (true, Vec::new())
    } else {
        (false, directions)
    }
}

// ---------------------------------------------------------------------------
// Markdown-file handoffs (submit_plan / submit_brief / submit_audit /
// submit_verdict). For PLANS the role writes the COMPLETE markdown to its
// project file via write_file and submits only a tiny {"file_path": ...};
// brief/audit/verdict are written in the response text (never inside the tool
// call — that caused JSON truncation). The swarm resolves the document from
// the file first (or the response text when that text IS the document),
// persists it as a project artifact, and parses the markdown back into the
// internal structs. The old JSON parsers remain as a fallback.
// ---------------------------------------------------------------------------

/// Resolves the handoff document for a role that called a `submit_*` tool:
/// 1. prefer the FILE at `file_path` when it exists and the response text is
///    not itself the full document (new contract: the plan lives in the file);
/// 2. otherwise use the role's final response text;
/// 3. persist the resolved document at `file_path` as a reviewable artifact.
/// True when `text` carries a handoff document anywhere in it (heading on its
/// own line). Models routinely put a short preamble ("All facts are gathered.
/// Writing the brief.") before the `# Summary` heading; if we only checked
/// `starts_with`, that preamble would make the response text look like "no
/// document", and `handoff_doc` would then read the STALE artifact file from a
/// previous run (`.kuda/brief.md` etc.) — the exact bug that made the Thinker
/// plan against an outdated, empty brief. Scanning every line fixes it.
fn looks_like_handoff_doc(text: &str) -> bool {
    text.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("# Goal") && text.contains("## Task")
            || t.starts_with("# Verdict")
            || t.starts_with("# Audit")
            || t.starts_with("# Summary")
            || t.starts_with("# Plan")
    })
}

fn handoff_doc(project_root: &Path, args_json: &str, fallback_text: &str) -> Result<String> {
    let v: serde_json::Value =
        serde_json::from_str(args_json).unwrap_or(serde_json::Value::Null);
    let file_path = v.get("file_path").and_then(|x| x.as_str()).unwrap_or("");

    let mut content = fallback_text.to_string();
    if !file_path.is_empty() {
        let abs = project_root.join(file_path);
        if let Ok(canon) = PathGuard::validate_path_in_scope(&abs, project_root) {
            if let Ok(s) = std::fs::read_to_string(&canon) {
                // The file is authoritative unless the response text itself
                // carries the full handoff doc (legacy/fallback path).
                if content.trim().is_empty() || !looks_like_handoff_doc(&content) {
                    content = s;
                }
            }
        }
    }
    if content.trim().is_empty() {
        return Err(AppError::General(format!(
            "Handoff document was empty (no response text and no readable file at '{}')",
            file_path
        )));
    }
    if !file_path.is_empty() {
        let abs = project_root.join(file_path);
        if let Ok(canon) = PathGuard::validate_path_in_scope(&abs, project_root) {
            if let Some(parent) = canon.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&canon, content.as_bytes());
        }
    }
    Ok(content)
}

/// Writes the final (already placeholder-expanded) handoff document to the
/// artifact file, so `.kuda/brief.md` on disk matches what the Thinker sees.
fn persist_handoff_artifact(project_root: &Path, args_json: &str, content: &str) {
    let v: serde_json::Value =
        serde_json::from_str(args_json).unwrap_or(serde_json::Value::Null);
    let file_path = v.get("file_path").and_then(|x| x.as_str()).unwrap_or("");
    if file_path.is_empty() {
        return;
    }
    let abs = project_root.join(file_path);
    if let Ok(canon) = PathGuard::validate_path_in_scope(&abs, project_root) {
        if let Some(parent) = canon.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&canon, content.as_bytes());
    }
}

/// Expands `[SNIPPET id="N"]` placeholder lines in the RLM Model's brief
/// markdown by fetching the exact captured bytes from the RLM kernel and
/// replacing each placeholder with a `--- path [start-end]` verbatim block.
///
/// The RLM Model captures regions with `_rlm_capture` (the kernel keeps the
/// exact bytes under an incrementing id) and references them by id instead of
/// retyping code — so the brief's snippets are byte-exact. Prose in the brief
/// is written by the model as usual; only the code blocks come from the kernel.
/// Unknown or unfetchable ids become a visible note instead of silently
/// dropping data. This is a best-effort enrichment: if the kernel is
/// unavailable the document is returned unchanged.
async fn resolve_snippet_placeholders(project_root: &Path, doc: &str) -> String {
    let ids = collect_snippet_placeholder_ids(doc);
    if ids.is_empty() {
        return doc.to_string();
    }
    let mut guard = match get_rlm_manager().get_or_spawn(project_root).await {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("RLM snippet expansion skipped (kernel unavailable): {}", e);
            return doc.to_string();
        }
    };
    let Some(proc) = guard.as_mut() else {
        return doc.to_string();
    };
    // Dump the ENTIRE snippet bank as a single-line JSON object (json.dumps
    // escapes newlines, so the stdout reader cannot split or corrupt it and the
    // sentinel stays on its own line). One round-trip for all captures.
    let code = "import json as _rj\nprint(_rj.dumps({str(_k): {'rel': _v['rel'], 'start': _v['start'], 'end': _v['end'], 'content': _v['content']} for _k, _v in _rlm_bank.items()}))\n";
    let out = match proc.execute_code(code, 10).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("RLM snippet bank fetch failed: {}", e);
            return doc.to_string();
        }
    };
    let json_line = out.lines().next().unwrap_or("");
    let bank: std::collections::HashMap<String, serde_json::Value> =
        match serde_json::from_str(json_line) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    "RLM snippet bank JSON unparseable ({}); got prefix: {:?}",
                    e,
                    &out[..out.len().min(160)]
                );
                return doc.to_string();
            }
        };
    let mut blocks: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    for id in ids {
        let key = id.to_string();
        let mut ok = false;
        if let Some(v) = bank.get(&key) {
            let rel = v.get("rel").and_then(|x| x.as_str()).unwrap_or("?");
            let start = v.get("start").and_then(|x| x.as_u64()).unwrap_or(0);
            let end = v.get("end").and_then(|x| x.as_u64()).unwrap_or(0);
            let content = v.get("content").and_then(|x| x.as_str()).unwrap_or("");
            if !content.is_empty() {
                blocks.insert(
                    id,
                    format!("--- {} [{}-{}]\n{}", rel, start, end, content),
                );
                ok = true;
            }
        }
        if !ok {
            blocks.insert(
                id,
                format!("(snippet id={} not found in kernel bank — check `_rlm_snippets()`)", id),
            );
        }
    }
    if !blocks.is_empty() {
        tracing::info!(
            "RLM snippet expansion: resolved {} of {} placeholders",
            blocks.len(),
            collect_snippet_placeholder_ids(doc).len()
        );
    }
    expand_snippet_placeholders(doc, &blocks)
}

/// Pure text transform: finds every `[SNIPPET id="N"]` / `[SNIPPET id=N]`
/// occurrence and swaps it for the matching block. Malformed or unknown ids
/// become a visible note; text outside placeholders is preserved unchanged.
fn expand_snippet_placeholders(
    doc: &str,
    blocks: &std::collections::HashMap<u32, String>,
) -> String {
    let mut out = String::with_capacity(doc.len() + 2048);
    let mut pos = 0usize;
    while pos < doc.len() {
        let rest = &doc[pos..];
        let Some(rel_start) = rest.find("[SNIPPET") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&doc[pos..pos + rel_start]);
        let after_marker = pos + rel_start + "[SNIPPET".len();
        let tail = &doc[after_marker..];
        let Some(close_rel) = tail.find(']') else {
            out.push_str(rest);
            break;
        };
        let inner = tail[..close_rel].trim();
        let id = inner
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .next()
            .and_then(|s| s.parse::<u32>().ok());
        match id {
            Some(n) => {
                let block = blocks
                    .get(&n)
                    .cloned()
                    .unwrap_or_else(|| format!("(snippet id={} not found)", n));
                out.push_str(&block);
                out.push('\n');
            }
            None => {
                // Malformed placeholder: keep the original text as-is.
                out.push_str("[SNIPPET");
                out.push_str(&tail[..close_rel + 1]);
            }
        }
        pos = after_marker + close_rel + 1;
    }
    out
}

/// Collects the distinct `[SNIPPET id="N"]` ids referenced in a document, in
/// first-occurrence order.
fn collect_snippet_placeholder_ids(doc: &str) -> Vec<u32> {
    let mut ids: Vec<u32> = Vec::new();
    let mut pos = 0usize;
    while pos < doc.len() {
        let rest = &doc[pos..];
        let Some(rel_start) = rest.find("[SNIPPET") else {
            break;
        };
        let after_marker = pos + rel_start + "[SNIPPET".len();
        let tail = &doc[after_marker..];
        let Some(close_rel) = tail.find(']') else {
            break;
        };
        let inner = tail[..close_rel].trim();
        if let Some(n) = inner
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .next()
            .and_then(|s| s.parse::<u32>().ok())
        {
            if !ids.contains(&n) {
                ids.push(n);
            }
        }
        pos = after_marker + close_rel + 1;
    }
    ids
}

/// Splits markdown into `(heading, body)` sections at every heading line.
fn md_sections(md: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cur: Option<(String, Vec<String>)> = None;
    for line in md.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            if let Some((h, body)) = cur.take() {
                out.push((h, body.join("\n")));
            }
            let heading = trimmed.trim_start_matches('#').trim().to_string();
            cur = Some((heading, Vec::new()));
        } else if let Some((_, body)) = cur.as_mut() {
            body.push(line.to_string());
        }
    }
    if let Some((h, body)) = cur.take() {
        out.push((h, body.join("\n")));
    }
    out
}

/// Finds the body of the first section whose heading contains `keyword`.
fn section_by<'a>(sections: &'a [(String, String)], keyword: &str) -> Option<&'a str> {
    let kw = keyword.to_lowercase();
    sections
        .iter()
        .find(|(h, _)| h.to_lowercase().contains(&kw))
        .map(|(_, b)| b.as_str())
}

/// Field keys that terminate a multi-line field's continuation block.
const MD_FIELD_KEYS: [&str; 5] = ["description:", "context:", "why:", "files:", "acceptance:"];

/// Normalizes one markdown line: strips list markers and bold so key detection
/// works for `- Description:`, `**Description**:`, and `- **Description**:`.
fn strip_line_md(line: &str) -> String {
    let mut l = line.trim();
    if let Some(stripped) = l.strip_prefix("- ").or_else(|| l.strip_prefix("* ")) {
        l = stripped.trim();
    }
    // Remove bold/italic markers without stripping the actual key name
    l.replace("**", "").replace("__", "").trim().to_string()
}

/// True when the line opens another task field (so a multi-line value stops).
fn is_field_start(line: &str) -> bool {
    let l = strip_line_md(line).to_lowercase();
    MD_FIELD_KEYS.iter().any(|k| l.starts_with(k))
}

/// Extracts a `key: value` (or `- **key**: value`) field from a markdown body.
/// Continuation lines (indented steps, bullets, code snippets) up until the
/// next field key are preserved, so multi-line task instructions survive
/// parsing and reach the executors intact.
fn md_field(body: &str, key: &str) -> Option<String> {
    let prefix = format!("{}:", key.to_lowercase());
    let lines: Vec<&str> = body.lines().collect();
    for (i, raw) in lines.iter().enumerate() {
        let l = strip_line_md(raw);
        if l.to_lowercase().starts_with(&prefix) {
            let mut parts: Vec<String> = vec![l[prefix.len()..].trim().to_string()];
            for cont in &lines[i + 1..] {
                if cont.trim().starts_with('#') || is_field_start(cont) {
                    break;
                }
                parts.push(cont.trim_end().to_string());
            }
            let v = parts.join("\n").trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

fn parse_plan_markdown(md: &str) -> Result<SwarmPlan> {
    let sections = md_sections(md);
    let goal = section_by(&sections, "goal")
        .or_else(|| section_by(&sections, "tujuan"))
        .map(|b| b.trim().to_string())
        .unwrap_or_default();
    let architecture = section_by(&sections, "arch")
        .or_else(|| section_by(&sections, "arsitektur"))
        .map(|b| b.trim().to_string());
    let risks = section_by(&sections, "risks")
        .or_else(|| section_by(&sections, "risiko"))
        .map(|b| b.trim().to_string());

    let mut tasks: Vec<SwarmPlanTask> = Vec::new();
    let mut next_id: u32 = 1;
    for (heading, body) in &sections {
        let hl = heading.to_lowercase();
        if !hl.starts_with("task") && !hl.starts_with("tugas") {
            continue;
        }
        let mut kind = "code".to_string();
        if let Some(open) = heading.find('[') {
            if let Some(close) = heading[open + 1..].find(']') {
                let k = heading[open + 1..open + 1 + close].to_lowercase();
                if k.contains("design") || k.contains("ui") || k.contains("css") || k.contains("styling") {
                    kind = "design".to_string();
                }
            }
        }
        let mut description = md_field(body, "description").unwrap_or_default();
        if description.trim().is_empty() {
            if let Some(idx) = heading.find(':') {
                description = heading[idx + 1..].trim().to_string();
            }
            if description.trim().is_empty() {
                description = body
                    .lines()
                    .map(|l| l.trim().trim_start_matches("- ").trim())
                    .find(|l| {
                        !l.is_empty()
                            && !l.starts_with("**")
                            && !l.to_lowercase().starts_with("files:")
                            && !l.to_lowercase().starts_with("acceptance:")
                            && !l.to_lowercase().starts_with("description:")
                    })
                    .unwrap_or("")
                    .to_string();
            }
        }
        let files: Vec<String> = md_field(body, "files")
            .unwrap_or_default()
            .split(',')
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty())
            .collect();
        let acceptance = md_field(body, "acceptance").unwrap_or_default();
        let context = md_field(body, "context").or_else(|| md_field(body, "why"));
        if description.trim().is_empty() && files.is_empty() && acceptance.trim().is_empty() {
            continue;
        }
        tasks.push(SwarmPlanTask {
            id: next_id,
            kind,
            description,
            context,
            files,
            acceptance,
        });
        next_id += 1;
    }
    if tasks.is_empty() {
        return Err(AppError::General(
            "Plan markdown contains no tasks".to_string(),
        ));
    }
    Ok(SwarmPlan {
        goal,
        architecture,
        risks,
        tasks,
    })
}

fn parse_verdict_markdown(md: &str) -> Result<Verdict> {
    let sections = md_sections(md);
    let mut passed = false;
    let mut summary = String::new();
    if let Some((heading, body)) = sections
        .iter()
        .find(|(h, _)| h.to_lowercase().contains("verdict"))
    {
        let hl = heading.to_lowercase();
        passed = if hl.contains("passed") && !hl.contains("fail") {
            true
        } else if hl.contains("fail") || hl.contains("reject") {
            false
        } else {
            let b = body.to_lowercase();
            b.contains("passed: yes")
                || b.contains("passed: true")
                || b.contains("complete") && !b.contains("incomplete")
        };
        summary = body
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("-"))
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();
    }

    let mut issues: Vec<VerdictIssue> = Vec::new();
    if let Some(body) = section_by(&sections, "issues") {
        for line in body.lines() {
            let l = line.trim().trim_start_matches("- ").trim();
            if l.is_empty() {
                continue;
            }
            let mut kind = "code".to_string();
            let mut desc = l.to_string();
            if let Some(open) = l.find('[') {
                if let Some(close) = l[open + 1..].find(']') {
                    let k = l[open + 1..open + 1 + close].to_lowercase();
                    if k.contains("design") || k.contains("ui") || k.contains("css") {
                        kind = "design".to_string();
                    }
                    desc = l[close + 2..].trim().to_string();
                }
            }
            if !desc.is_empty() {
                issues.push(VerdictIssue {
                    description: desc,
                    kind,
                });
            }
        }
    }
    Ok(Verdict {
        passed,
        summary,
        issues,
    })
}

fn parse_audit_markdown(md: &str) -> Result<ContextAudit> {
    let sections = md_sections(md);
    let mut complete = false;
    let mut summary = String::new();
    if let Some((heading, body)) = sections
        .iter()
        .find(|(h, _)| h.to_lowercase().contains("audit"))
    {
        let hl = heading.to_lowercase();
        complete = hl.contains("complete") && !hl.contains("incomplete");
        summary = body
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("-"))
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();
    }

    let mut missing: Vec<AuditGap> = Vec::new();
    let missing_body = section_by(&sections, "missing")
        .or_else(|| section_by(&sections, "kekurangan"))
        .or_else(|| section_by(&sections, "gap"));

    if let Some(body) = missing_body {
        for line in body.lines() {
            let l = line.trim().trim_start_matches("- ").trim_start_matches("* ").trim();
            if l.is_empty() {
                continue;
            }
            let mut what = l.to_string();
            let mut where_path = String::new();
            let mut why_needed = String::new();
            if let Some(idx) = l.find('(') {
                let tail = &l[idx + 1..];
                let before = l[..idx].trim();
                what = before.to_string();
                let stripped = tail
                    .strip_prefix("needed for:")
                    .or_else(|| tail.strip_prefix("needed for :"));
                if let Some(w) = stripped {
                    why_needed = w.trim_end_matches(')').trim().to_string();
                } else if let Some(end) = tail.find(')') {
                    why_needed = tail[..end].trim().to_string();
                }
            }
            for sep in ["—", "–", " - "] {
                if let Some(i) = what.find(sep) {
                    where_path = what[i + sep.len()..].trim().to_string();
                    what = what[..i].trim().to_string();
                    break;
                }
            }
            missing.push(AuditGap {
                what,
                where_path,
                why_needed,
            });
        }
    }

    if !complete && missing.is_empty() {
        missing.push(AuditGap {
            what: if !summary.is_empty() {
                summary.clone()
            } else {
                "Brief incomplete per verifier audit".to_string()
            },
            where_path: String::new(),
            why_needed: "verification of research completeness".to_string(),
        });
    }

    Ok(ContextAudit {
        complete,
        summary,
        missing,
    })
}

fn parse_brief_markdown(md: &str) -> Result<ResearchBrief> {
    let sections = md_sections(md);

    let summary = section_by(&sections, "summary")
        .map(|b| b.trim().to_string())
        .unwrap_or_default();
    let conventions = section_by(&sections, "conventions")
        .map(|b| b.trim().to_string())
        .unwrap_or_default();

    let key_files: Vec<BriefFile> = section_by(&sections, "key files")
        .map(|b| {
            b.lines()
                .filter_map(|line| {
                    let l = line.trim().trim_start_matches("- ").trim();
                    if l.is_empty() || l.starts_with("---") {
                        return None;
                    }
                    let mut path = l.to_string();
                    let mut why = String::new();
                    let mut syms: Vec<String> = Vec::new();
                    if let Some(idx) = l.find('(') {
                        let tail = &l[idx + 1..];
                        let before = l[..idx].trim();
                        if let Some(s) = tail.strip_prefix("symbols:") {
                            syms = s
                                .trim_end_matches(')')
                                .split(',')
                                .map(|x| x.trim().to_string())
                                .filter(|x| !x.is_empty())
                                .collect();
                        }
                        for sep in ["—", "–", " - "] {
                            if let Some(i) = before.find(sep) {
                                path = before[..i].trim().to_string();
                                why = before[i + sep.len()..].trim().to_string();
                                break;
                            }
                        }
                        if path.is_empty() {
                            path = before.trim().to_string();
                        }
                    } else {
                        for sep in ["—", "–", " - "] {
                            if let Some(i) = l.find(sep) {
                                path = l[..i].trim().to_string();
                                why = l[i + sep.len()..].trim().to_string();
                                break;
                            }
                        }
                    }
                    if path.is_empty() {
                        None
                    } else {
                        Some(BriefFile {
                            path,
                            why,
                            key_symbols: syms,
                        })
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let relevant_snippets: Vec<BriefSnippet> = section_by(&sections, "relevant snippets")
        .map(|b| {
            let mut snippets: Vec<BriefSnippet> = Vec::new();
            let mut cur: Option<(String, String, Vec<String>)> = None;
            for line in b.lines() {
                let l = line.trim();
                if l.starts_with("---") {
                    if let Some((path, lines, content)) = cur.take() {
                        snippets.push(BriefSnippet {
                            path,
                            lines,
                            content: content.join("\n"),
                        });
                    }
                    let rest = l[3..].trim();
                    let mut path = rest.to_string();
                    let mut lines_s = String::new();
                    if let Some(idx) = rest.find('[') {
                        path = rest[..idx].trim().to_string();
                        if let Some(end) = rest[idx + 1..].find(']') {
                            lines_s = rest[idx + 1..idx + 1 + end].trim().to_string();
                        }
                    }
                    cur = Some((path, lines_s, Vec::new()));
                } else if let Some((_, _, content)) = cur.as_mut() {
                    content.push(line.to_string());
                }
            }
            if let Some((path, lines, content)) = cur.take() {
                snippets.push(BriefSnippet {
                    path,
                    lines,
                    content: content.join("\n"),
                });
            }
            snippets
        })
        .unwrap_or_default();

    let risks_unknowns: Vec<String> = section_by(&sections, "risks")
        .map(|b| {
            b.lines()
                .map(|l| l.trim().trim_start_matches("- ").trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let external_pulls: Vec<BriefExternal> = section_by(&sections, "external pulls")
        .map(|b| {
            b.lines()
                .filter_map(|line| {
                    let l = line.trim().trim_start_matches("- ").trim();
                    if l.is_empty() {
                        return None;
                    }
                    let mut path = l.to_string();
                    let mut why = String::new();
                    let mut verified_safe = false;
                    if let Some(idx) = l.find('(') {
                        let tail = &l[idx + 1..];
                        let before = l[..idx].trim();
                        verified_safe = tail
                            .trim_end_matches(')')
                            .to_lowercase()
                            .contains("yes")
                            || tail
                                .trim_end_matches(')')
                                .to_lowercase()
                                .contains("true");
                        for sep in ["—", "–", " - "] {
                            if let Some(i) = before.find(sep) {
                                path = before[..i].trim().to_string();
                                why = before[i + sep.len()..].trim().to_string();
                                break;
                            }
                        }
                        if path.is_empty() {
                            path = before.trim().to_string();
                        }
                    }
                    if path.is_empty() {
                        None
                    } else {
                        Some(BriefExternal {
                            path,
                            why,
                            verified_safe,
                        })
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(ResearchBrief {
        summary,
        key_files,
        relevant_snippets,
        conventions,
        risks_unknowns,
        external_pulls,
    })
}

/// Document-level parsers: try the legacy JSON shape first, then markdown.
fn parse_plan_doc(doc: &str) -> Result<SwarmPlan> {
    let t = doc.trim();
    if t.starts_with('{') {
        parse_plan(t)
    } else {
        parse_plan_markdown(t)
    }
}

fn parse_verdict_doc(doc: &str) -> Result<Verdict> {
    let t = doc.trim();
    if t.starts_with('{') {
        parse_verdict(t)
    } else {
        parse_verdict_markdown(t)
    }
}

fn parse_audit_doc(doc: &str) -> Result<ContextAudit> {
    let t = doc.trim();
    if t.starts_with('{') {
        parse_audit(t)
    } else {
        parse_audit_markdown(t)
    }
}

fn parse_brief_doc(doc: &str) -> Result<ResearchBrief> {
    let t = doc.trim();
    if t.starts_with('{') {
        parse_brief(t)
    } else {
        parse_brief_markdown(t)
    }
}

/// Renders a plan back to the markdown template so roles in the shared context
/// read plans as markdown (never JSON).
fn render_plan_markdown(plan: &SwarmPlan) -> String {
    let mut s = String::from("# Goal\n");
    s.push_str(&plan.goal);
    s.push('\n');
    if let Some(arch) = plan.architecture.as_deref() {
        if !arch.trim().is_empty() {
            s.push_str("\n## Architecture\n");
            s.push_str(arch.trim());
            s.push('\n');
        }
    }
    for t in &plan.tasks {
        s.push_str(&format!(
            "\n## Task {} [{}]\n- Description: {}\n",
            t.id,
            t.kind,
            t.description
        ));
        if let Some(ctx) = t.context.as_deref() {
            if !ctx.trim().is_empty() {
                s.push_str(&format!("- Context: {}\n", ctx.trim()));
            }
        }
        s.push_str(&format!(
            "- Files: {}\n- Acceptance: {}\n",
            t.files.join(", "),
            t.acceptance
        ));
    }
    if let Some(risks) = plan.risks.as_deref() {
        if !risks.trim().is_empty() {
            s.push_str("\n## Risks / Unknowns\n");
            s.push_str(risks.trim());
            s.push('\n');
        }
    }
    s
}

/// Renders the validated brief + audit into a compact text digest that the
/// slim Thinker consumes as ground truth (no raw exploration, no tool dumps).
fn format_brief_digest(brief: &ResearchBrief, audit: &ContextAudit) -> String {
    let mut s = String::new();
    s.push_str("## SUMMARY\n");
    s.push_str(&brief.summary.trim());
    s.push('\n');

    if !brief.key_files.is_empty() {
        s.push_str("\n## KEY FILES\n");
        for f in &brief.key_files {
            let syms = if f.key_symbols.is_empty() {
                String::from("(none noted)")
            } else {
                f.key_symbols.join(", ")
            };
            s.push_str(&format!("- {} — {}\n  symbols: {}\n", f.path, f.why, syms));
        }
    }

    if !brief.relevant_snippets.is_empty() {
        s.push_str("\n## RELEVANT SNIPPETS\n");
        for snip in &brief.relevant_snippets {
            let lines = if snip.lines.is_empty() {
                String::from("(whole)")
            } else {
                snip.lines.clone()
            };
            let body = snip.content.trim();
            s.push_str(&format!("--- {} [{}]\n{}\n", snip.path, lines, body));
        }
    }

    if !brief.conventions.is_empty() {
        s.push_str("\n## CONVENTIONS\n");
        s.push_str(brief.conventions.trim());
        s.push('\n');
    }

    if !brief.risks_unknowns.is_empty() {
        s.push_str("\n## RISKS / UNKNOWNS\n");
        for r in &brief.risks_unknowns {
            s.push_str(&format!("- {}\n", r));
        }
    }

    if !brief.external_pulls.is_empty() {
        s.push_str("\n## EXTERNAL PULLS (outside project)\n");
        for e in &brief.external_pulls {
            s.push_str(&format!(
                "- {} — {} [verified_safe={}]\n",
                e.path,
                e.why,
                if e.verified_safe { "yes" } else { "no" }
            ));
        }
    }

    s.push_str("\n## VERIFIER AUDIT\n");
    s.push_str(&format!(
        "complete={} — {}",
        if audit.complete { "yes" } else { "no" },
        audit.summary.trim()
    ));
    if !audit.missing.is_empty() {
        s.push_str("\nMissing: ");
        s.push_str(&audit
            .missing
            .iter()
            .map(|g| format!("{} ({})", g.what, g.where_path))
            .collect::<Vec<_>>()
            .join("; "));
    }
    s
}

/// Formats a timestamp as ISO 8601 with local timezone offset, or "unknown".
fn format_ts(ts: Option<chrono::DateTime<chrono::Local>>) -> String {
    match ts {
        Some(t) => t.format("%Y-%m-%dT%H:%M%:z").to_string(),
        None => "unknown".to_string(),
    }
}

/// Renders a relative age ("just now", "12 minutes ago", ...) for time-aware
/// prompts so the model can judge how fresh a cached research actually is.
fn format_relative(ts: chrono::DateTime<chrono::Local>) -> String {
    let secs = chrono::Local::now()
        .signed_duration_since(ts)
        .num_seconds()
        .max(0);
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{} minutes ago", secs / 60)
    } else if secs < 86400 {
        format!("{} hours ago", secs / 3600)
    } else {
        format!("{} days ago", secs / 86400)
    }
}

/// Labels a cached research with its age, or empty when un-datable.
fn label_prior_research(c: &ProjectCache) -> String {
    match c.manifest.as_ref() {
        Some(m) => format!(" — {}", format_relative(m.generated_at)),
        None => String::new(),
    }
}

/// Pre-warms the kernel with the previous session's inventory, skipping files
/// that changed since (their current state must be read explicitly).
async fn prewarm_cache(
    cached: &Option<ProjectCache>,
    manifest_diff: &Option<ManifestDiff>,
    project_root: &std::path::Path,
) {
    let Some(c) = cached.as_ref() else { return };
    let Some(inv) = c.inventory.as_ref() else { return };
    let changed_list = manifest_diff
        .as_ref()
        .map(|d| d.all_changed())
        .unwrap_or_default();
    let changed: HashSet<&String> = changed_list.iter().collect();
    let abs: Vec<String> = inv
        .loaded_paths
        .iter()
        .filter(|p| !changed.contains(p))
        .map(|p| project_root.join(p).to_string_lossy().to_string())
        .collect();
    if abs.is_empty() {
        return;
    }
    match get_rlm_manager().prewarm(project_root, &abs).await {
        Ok(n) => tracing::debug!("RLM prewarm loaded {} cached files", n),
        Err(e) => tracing::warn!("RLM prewarm failed: {}", e),
    }
}

fn normalize_tasks(mut tasks: Vec<SwarmPlanTask>) -> Vec<SwarmPlanTask> {
    for (i, task) in tasks.iter_mut().enumerate() {
        // Reassign sequential ids so duplicate ids emitted by a model never occur.
        task.id = (i + 1) as u32;
        if !task.kind.eq_ignore_ascii_case("design") {
            task.kind = "code".into();
        } else {
            task.kind = "design".into();
        }
    }
    tasks
}

fn build_executor_brief(plan: &SwarmPlan, task: &SwarmPlanTask) -> String {
    let context_line = task
        .context
        .as_deref()
        .filter(|c| !c.trim().is_empty())
        .map(|c| format!("WHY / CONTEXT: {}\n", c.trim()))
        .unwrap_or_default();
    let architecture_line = plan
        .architecture
        .as_deref()
        .filter(|a| !a.trim().is_empty())
        .map(|a| format!("\n== ARCHITECTURE CONTRACT (your task must implement this) ==\n{}\n", a.trim()))
        .unwrap_or_default();
    format!(
        "[EXECUTOR TASK BRIEF]\nExecute exactly this task and nothing else.\n\n\
         GOAL: {}\n\nTASK #{} (kind: {})\nDESCRIPTION: {}\n{}\
         FILES EXPECTED: {}\nACCEPTANCE: {}\n{}\n\n\
         FULL PLAN (context only, do not execute other tasks):\n{}",
        plan.goal,
        task.id,
        task.kind,
        task.description,
        context_line,
        if task.files.is_empty() { "(discover from context)".to_string() } else { task.files.join(", ") },
        task.acceptance,
        architecture_line,
        render_plan_markdown(plan)
    )
}

/// Max files scanned by the tree snapshot (bounds cost on huge repos).
/// Bounded well below the previous 3000×2 MB worst case (~6 GB per map) so the
/// diff pipeline cannot OOM the IDE on large trees.
const MAX_SNAPSHOT_FILES: usize = 800;
/// Files larger than this are skipped by the tree snapshot (not diffed).
const MAX_SNAPSHOT_FILE_BYTES: u64 = 256 * 1024;
/// Hard cap on the TOTAL bytes retained by one snapshot, so even many mid-size
/// files cannot balloon the process memory (the diff report is truncated to
/// `MAX_REPORT_CHARS` anyway, so oversized snapshots add no signal).
const MAX_SNAPSHOT_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// Directories the tree snapshot skips (dependency/build output trees).
fn is_heavy_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "node_modules" | "target" | "dist" | "build" | ".next" | "out" | "vendor"
            | ".venv" | "__pycache__" | ".cache" | "coverage" | "Pods" | ".pytest_cache"
    )
}

/// Snapshots the readable text files of the whole project tree, keyed by
/// relative path. Used to diff an executor's work against the tree as it was
/// BEFORE the executor ran, so the report catches ANY change — including edits
/// performed through `run_command` (which bypass the checkpoint system).
fn snapshot_project_tree(tool_ctx: &ToolContext) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut total_bytes: u64 = 0;
    let walker = ignore::WalkBuilder::new(&tool_ctx.project_root)
        .hidden(false)
        .git_ignore(true)
        .filter_entry(|e| {
            e.file_type().map_or(true, |ft| {
                !ft.is_dir() || !is_heavy_dir(e.file_name().to_string_lossy().as_ref())
            })
        })
        .build();

    for entry in walker.flatten() {
        if map.len() >= MAX_SNAPSHOT_FILES {
            break;
        }
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(meta) = path.metadata() else {
            continue;
        };
        if meta.len() > MAX_SNAPSHOT_FILE_BYTES {
            continue;
        }
        let rel = path.strip_prefix(&tool_ctx.project_root).unwrap_or(path);
        let rel_str = rel.to_string_lossy().to_string();
        if rel_str.is_empty() {
            continue;
        }
        // Scope enforcement: unreadable / out-of-scope files are skipped.
        if let Ok(payload) = FileSystemIO::read_file(path, &tool_ctx.project_root, None, None) {
            total_bytes = total_bytes.saturating_add(payload.content.len() as u64);
            if total_bytes > MAX_SNAPSHOT_TOTAL_BYTES {
                break;
            }
            map.insert(rel_str, payload.content);
        }
    }
    map
}

/// Computes a compact text diff report of everything that changed in the project
/// tree between `before` and the current state. This is the ONLY thing about
/// executor work that enters the shared context.
fn build_tree_diff_report(tool_ctx: &ToolContext, before: &HashMap<String, String>) -> String {
    let after = snapshot_project_tree(tool_ctx);

    let mut keys: Vec<&String> = before.keys().chain(after.keys()).collect();
    keys.sort();
    keys.dedup();

    let mut file_reports: Vec<String> = Vec::new();
    let mut change_count = 0usize;

    for rel in keys {
        let old = before.get(rel);
        let new = after.get(rel);
        let report = match (old, new) {
            (None, None) => None,
            (Some(o), Some(n)) if o == n => None,
            (Some(o), Some(n)) => {
                let diff = DiffCalculator::compute_diff(std::path::PathBuf::from(rel), o, n);
                let mut body = String::new();
                let mut shown = 0usize;
                for change in &diff.changes {
                    match change.kind {
                        ChangeKind::Insert if shown < MAX_DIFF_LINES_PER_FILE => {
                            shown += 1;
                            body.push_str(&format!(
                                "+{:>5}| {}\n",
                                change.new_line.unwrap_or(0) + 1,
                                change.content.trim_end()
                            ));
                        }
                        ChangeKind::Delete if shown < MAX_DIFF_LINES_PER_FILE => {
                            shown += 1;
                            body.push_str(&format!(
                                "-{:>5}| {}\n",
                                change.old_line.unwrap_or(0) + 1,
                                change.content.trim_end()
                            ));
                        }
                        ChangeKind::Equal => {}
                        _ => {}
                    }
                }
                if shown < diff.insertions + diff.deletions {
                    body.push_str(&format!(
                        "… ({} more changed lines)\n",
                        (diff.insertions + diff.deletions) - shown
                    ));
                }
                change_count += diff.insertions + diff.deletions;
                Some(format!(
                    "--- {} (+{} -{})\n{}",
                    rel, diff.insertions, diff.deletions, body
                ))
            }
            (None, Some(n)) => {
                let lines: Vec<&str> = n.lines().collect();
                change_count += lines.len();
                let mut body = String::new();
                for (i, line) in lines.iter().take(MAX_DIFF_LINES_PER_FILE).enumerate() {
                    body.push_str(&format!("+{:>5}| {}\n", i + 1, line));
                }
                if lines.len() > MAX_DIFF_LINES_PER_FILE {
                    body.push_str(&format!("… (+{} more lines)\n", lines.len() - MAX_DIFF_LINES_PER_FILE));
                }
                Some(format!("--- {} (NEW FILE, {} lines)\n{}", rel, lines.len(), body))
            }
            (Some(_), None) => {
                change_count += 1;
                Some(format!("--- {} (DELETED)", rel))
            }
        };
        if let Some(r) = report {
            file_reports.push(r);
        }
    }

    let mut report = String::from("Changed files: ");
    if file_reports.is_empty() {
        report.push_str("none (no file changes detected)");
        return report;
    }
    report.push_str(&format!(
        "{} file(s), {} changed line(s)\n\n{}",
        file_reports.len(),
        change_count,
        file_reports.join("\n")
    ));
    report
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}… (truncated)", s.chars().take(max).collect::<String>())
    }
}

/// Renders the turn's ledger into ONE assistant message (name = "ledger") that
/// becomes the context for the NEXT turn. Every segment is budget-capped; empty
/// segments are written as "—" so the structure stays stable for prompt caching.
pub fn build_ledger_message(ledger: &TurnLedger) -> Message {
    let seg = |v: Option<&String>, max: usize| {
        v.map(|s| truncate_chars(s, max)).unwrap_or_else(|| "—".to_string())
    };
    let mut content = String::from("[TURN LEDGER]\n");
    content.push_str(&format!("[RESEARCH BRIEF]\n{}\n", seg(ledger.brief_digest.as_ref(), LEDGER_BRIEF_CHARS)));
    content.push_str(&format!("[PLAN]\n{}\n", seg(ledger.plan_markdown.as_ref(), LEDGER_PLAN_CHARS)));
    if let Some(status) = ledger.plan_status.as_deref() {
        content.push_str(&format!("[PLAN STATUS] {}\n", status));
    }
    content.push_str(&format!("[EXECUTION REVIEW]\n{}\n", seg(ledger.execution_review.as_ref(), LEDGER_EXEC_CHARS)));
    let answer = if ledger.final_answer.trim().is_empty() {
        "Run selesai tanpa jawaban final — kemungkinan gagal di tengah pipeline atau tidak ada output yang dihasilkan.".to_string()
    } else {
        ledger.final_answer.clone()
    };
    content.push_str(&format!("[FINAL ANSWER]\n{}", truncate_chars(&answer, LEDGER_ANSWER_CHARS)));
    Message {
        role: crate::agent::llm_client::MessageRole::Assistant,
        content,
        name: Some("ledger".to_string()),
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        created_at: Some(chrono::Local::now()),
    }
}

/// Condenses the executor notes + verdict into the compact `[EXECUTION REVIEW]`
/// ledger segment. Not the 6000-char shared report — just file oneliners plus
/// the verdict and (on failure) the top issues.
fn build_execution_review_ledger(
    exec_notes: &[String],
    verdict: &Option<Verdict>,
    verdict_state: &str,
) -> String {
    let mut s = String::new();
    if exec_notes.is_empty() {
        s.push_str("[EXEC] (no executor tasks ran — plan was answered directly or cancelled)\n");
    } else {
        for note in exec_notes {
            s.push_str(&format!("{}\n", note));
        }
    }

    match verdict {
        Some(v) if v.passed => {
            s.push_str(&format!("[VERDICT] PASSED — {}\n", v.summary.trim()));
        }
        Some(v) => {
            s.push_str(&format!(
                "[VERDICT] FAILED — {}\n",
                truncate_chars(v.summary.trim(), 200)
            ));
            for issue in v.issues.iter().take(3) {
                s.push_str(&format!(
                    "[ISSUE] [{}] {}\n",
                    issue.kind,
                    truncate_chars(&issue.description, 120)
                ));
            }
        }
        None => {
            s.push_str(&format!("[VERDICT] UNVERIFIED — {}\n", verdict_state));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_plan_nested_and_flat() {
        let nested = r#"{"plan": {"goal": "g", "tasks": [{"id": 1, "kind": "code", "description": "d"}]}}"#;
        let flat = r#"{"goal": "g", "tasks": [{"id": 1, "kind": "design", "description": "d"}]}"#;
        let p1 = parse_plan(nested).unwrap();
        let p2 = parse_plan(flat).unwrap();
        assert_eq!(p1.tasks.len(), 1);
        assert_eq!(p1.tasks[0].kind, "code");
        assert_eq!(p2.tasks[0].kind, "design");
    }

    #[test]
    fn test_normalize_tasks_defaults() {
        let tasks = vec![
            SwarmPlanTask { id: 0, kind: "CODE".into(), description: "a".into(), context: None, files: vec![], acceptance: "".into() },
            SwarmPlanTask { id: 0, kind: "Design".into(), description: "b".into(), context: None, files: vec![], acceptance: "".into() },
        ];
        let norm = normalize_tasks(tasks);
        assert_eq!(norm[0].id, 1);
        assert_eq!(norm[0].kind, "code");
        assert_eq!(norm[1].id, 2);
        assert_eq!(norm[1].kind, "design");
    }

    #[test]
    fn test_parse_verdict() {
        let args = r#"{"verdict": {"passed": false, "summary": "s", "issues": [{"description": "missing button", "kind": "design"}]}}"#;
        let v = parse_verdict(args).unwrap();
        assert!(!v.passed);
        assert_eq!(v.issues.len(), 1);
    }

    #[test]
    fn test_parse_audit_nested_and_flat() {
        let nested = r#"{"audit": {"complete": false, "summary": "gaps", "missing": [{"what": "store file", "where": "src/store/agent.ts", "why_needed": "task 1"}]}}"#;
        let flat = r#"{"complete": true, "summary": "ok", "missing": []}"#;
        let a1 = parse_audit(nested).unwrap();
        let a2 = parse_audit(flat).unwrap();
        assert!(!a1.complete);
        assert_eq!(a1.missing.len(), 1);
        assert_eq!(a1.missing[0].where_path, "src/store/agent.ts");
        assert!(a2.complete);
        assert!(a2.missing.is_empty());
    }

    #[test]
    fn test_parse_brief_nested_and_flat() {
        let nested = r#"{"brief": {"summary": "s", "key_files": [{"path": "src/a.rs", "why": "core", "key_symbols": ["foo"]}], "conventions": "camelCase", "risks_unknowns": ["x"], "external_pulls": [{"path": "/etc/cfg", "why": "config", "verified_safe": true}]}}"#;
        let flat = r#"{"summary": "s2"}"#;
        let b1 = parse_brief(nested).unwrap();
        let b2 = parse_brief(flat).unwrap();
        assert_eq!(b1.summary, "s");
        assert_eq!(b1.key_files.len(), 1);
        assert_eq!(b1.key_files[0].key_symbols, vec!["foo".to_string()]);
        assert_eq!(b1.conventions, "camelCase");
        assert_eq!(b1.external_pulls.len(), 1);
        assert!(b1.external_pulls[0].verified_safe);
        assert_eq!(b2.summary, "s2");
        assert!(b2.key_files.is_empty());
    }

    #[test]
    fn test_parse_plan_markdown() {
        let md = r#"# Goal
Build a booking web app

## Task 1 [code]
- Description: Create the axum router and handlers
- Files: src/main.rs, src/routes.rs
- Acceptance: cargo check passes

## Task 2 [design]
- Description: Style the booking forms
- Files: templates/index.html, static/style.css
- Acceptance: templates render with the layout"#;
        let plan = parse_plan_markdown(md).unwrap();
        assert_eq!(plan.goal, "Build a booking web app");
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[0].kind, "code");
        assert_eq!(plan.tasks[0].id, 1);
        assert_eq!(plan.tasks[0].files, vec!["src/main.rs", "src/routes.rs"]);
        assert!(plan.tasks[0].acceptance.contains("cargo check"));
        assert_eq!(plan.tasks[1].kind, "design");
        assert_eq!(plan.tasks[1].id, 2);
    }

    #[test]
    fn test_parse_plan_markdown_with_architecture_and_risks() {
        let md = r#"# Goal
Build an axum web app

## Architecture
- Components: main.rs (routes), handlers.rs, storage.rs
- Data flow: request -> handler -> storage -> response
- Concurrency: tokio + AppState with Mutex
- Error handling: ApiError mapping to 400/500

## Task 1 [code]
- Description: Set up the axum server with AppState
- Context: The server serves static files + JSON API; AppState holds the in-memory order store
- Files: src/main.rs
- Acceptance: cargo check passes

## Risks / Unknowns
- cargo/rustc must be installed
- crates.io must be reachable
"#;
        let plan = parse_plan_markdown(md).unwrap();
        let arch = plan.architecture.as_deref().unwrap();
        assert!(arch.contains("Data flow"), "architecture: {}", arch);
        assert!(arch.contains("Concurrency"));
        let risks = plan.risks.as_deref().unwrap();
        assert!(risks.contains("cargo"));
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(
            plan.tasks[0].context.as_deref(),
            Some("The server serves static files + JSON API; AppState holds the in-memory order store")
        );

        // Render round-trip keeps the architecture + risks + context sections.
        let rendered = render_plan_markdown(&plan);
        assert!(rendered.contains("## Architecture"), "{}", rendered);
        assert!(rendered.contains("Data flow"));
        assert!(rendered.contains("## Risks / Unknowns"));
        assert!(rendered.contains("cargo/rustc"));
        assert!(rendered.contains("- Context: The server serves static files"));
    }

    #[test]
    fn test_parse_plan_markdown_multiline_fields() {
        let md = r#"# Goal
Build an axum web app

## Task 1 [code]
- Description: Add the booking endpoint
  1. In src/handlers.rs, inside `fn booking(ctx)`, add the insert call
  2. Map the error to 500 via ApiError
  Use `seed` from src/db.rs exactly as in the brief snippet.
- Context: BookingRepo is in src/db.rs; do NOT touch src/auth.rs
- Files: src/handlers.rs, src/db.rs
- Acceptance: cargo test booking passes

## Risks / Unknowns
- cargo installed"#;
        let plan = parse_plan_markdown(md).unwrap();
        assert_eq!(plan.tasks.len(), 1);
        let t = &plan.tasks[0];
        assert!(
            t.description.contains("1. In src/handlers.rs"),
            "description: {}",
            t.description
        );
        assert!(t.description.contains("2. Map the error"));
        assert!(t.description.contains("Use `seed`"));
        assert!(t.description.contains("Add the booking endpoint"));
        assert_eq!(
            t.context.as_deref(),
            Some("BookingRepo is in src/db.rs; do NOT touch src/auth.rs")
        );
        assert_eq!(t.acceptance, "cargo test booking passes");
    }

    #[test]
    fn test_build_executor_brief_renders_markdown_not_json() {
        let plan = SwarmPlan {
            goal: "Build a booking web app".into(),
            architecture: Some("Data flow: request -> handler -> store -> response".into()),
            risks: None,
            tasks: vec![SwarmPlanTask {
                id: 1,
                kind: "code".into(),
                description: "Add the endpoint\n  1. edit src/main.rs".into(),
                context: Some("do not touch auth".into()),
                files: vec!["src/main.rs".into()],
                acceptance: "cargo check passes".into(),
            }],
        };
        let brief = build_executor_brief(&plan, &plan.tasks[0]);
        assert!(brief.contains("ARCHITECTURE CONTRACT"), "{}", brief);
        assert!(brief.contains("## Task 1 [code]"));
        assert!(brief.contains("do not touch auth"));
        assert!(brief.contains("1. edit src/main.rs"));
        assert!(!brief.contains("{\""), "brief must not be raw JSON");
    }

    #[test]
    fn test_parse_verdict_markdown() {
        let md = r#"# Verdict: FAILED
Booking form has missing CSRF field

## Issues
- [code] booking insert does not check duplicate times
- [design] form lacks inline validation styles"#;
        let v = parse_verdict_markdown(md).unwrap();
        assert!(!v.passed);
        assert!(v.summary.contains("CSRF"));
        assert_eq!(v.issues.len(), 2);
        assert_eq!(v.issues[0].kind, "code");
        assert_eq!(v.issues[1].kind, "design");

        let ok = parse_verdict_markdown("# Verdict: PASSED\nAll tasks verified.").unwrap();
        assert!(ok.passed);
        assert!(ok.issues.is_empty());
    }

    #[test]
    fn test_parse_audit_markdown() {
        let incomplete = r#"# Audit: INCOMPLETE
Missing session handling details

## Missing
- session middleware — src/main.rs (needed for: login flow)
- argon2 setup — Cargo.toml (needed for: password hashing)"#;
        let a = parse_audit_markdown(incomplete).unwrap();
        assert!(!a.complete);
        assert_eq!(a.missing.len(), 2);
        assert_eq!(a.missing[0].what, "session middleware");
        assert_eq!(a.missing[0].where_path, "src/main.rs");
        assert_eq!(a.missing[1].why_needed, "password hashing");

        let ok = parse_audit_markdown("# Audit: COMPLETE\nAll good.").unwrap();
        assert!(ok.complete);
        assert!(ok.missing.is_empty());
    }

    #[test]
    fn test_parse_brief_markdown() {
        let md = r#"# Summary
Cleaning service web app in Rust.

# Key Files
- src/db.rs — data layer (symbols: seed:12, BookingRepo:40-56)
- src/main.rs — web layer

# Relevant Snippets
--- src/db.rs [1-20]
use rusqlite::Connection;

# Conventions
snake_case, rusqlite with Arc<Mutex>

# Risks / Unknowns
- argon2 error type not StdError

# External Pulls
- /etc/cleaning.conf — service config (safe: yes)"#;
        let b = parse_brief_markdown(md).unwrap();
        assert!(b.summary.contains("Cleaning service"));
        assert_eq!(b.key_files.len(), 2);
        assert_eq!(b.key_files[0].path, "src/db.rs");
        assert_eq!(b.key_files[0].key_symbols, vec!["seed:12", "BookingRepo:40-56"]);
        assert_eq!(b.relevant_snippets.len(), 1);
        assert_eq!(b.relevant_snippets[0].lines, "1-20");
        assert!(b.conventions.contains("rusqlite"));
        assert_eq!(b.risks_unknowns.len(), 1);
        assert_eq!(b.external_pulls.len(), 1);
        assert!(b.external_pulls[0].verified_safe);
    }

    #[test]
    fn test_handoff_doc_persists_final_text_to_file() {
        let temp_dir = std::env::temp_dir().join("kuda_handoff_doc_test");
        let project_root = temp_dir.join("project");
        let _ = std::fs::create_dir_all(&project_root);

        let args = r#"{"file_path": ".kuda/plan.md"}"#;
        let fallback = "# Goal\nplan from response text";
        let doc = handoff_doc(&project_root, args, fallback).unwrap();
        assert_eq!(doc, fallback);

        // The document must be persisted as a project artifact.
        let persisted = project_root.join(".kuda/plan.md");
        assert!(persisted.exists());
        assert_eq!(std::fs::read_to_string(&persisted).unwrap(), fallback);

        // Empty response text falls back to reading the file written by the role.
        let doc2 = handoff_doc(&project_root, args, "").unwrap();
        assert_eq!(doc2, fallback);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_looks_like_handoff_doc_allows_preamble() {
        // A preamble before the heading must still be recognized as a handoff
        // doc; otherwise handoff_doc reads the STALE `.kuda/` artifact file from
        // a previous run and the freshly-written brief never reaches the Thinker.
        assert!(looks_like_handoff_doc(
            "All facts are gathered. Writing the brief.\n\n# Summary\n...",
        ));
        assert!(looks_like_handoff_doc(
            "Let me audit this brief.\n# Audit: INCOMPLETE\n## Missing\n- x",
        ));
        assert!(looks_like_handoff_doc("# Verdict: PASSED\n..."));
        assert!(looks_like_handoff_doc("Here is the plan.\n# Goal\n...\n## Task 1"));
        assert!(!looks_like_handoff_doc("Just answering normally."));
        assert!(!looks_like_handoff_doc(""));
    }

    #[test]
    fn test_parse_brief_doc_json_fallback() {
        let json = r#"{"brief": {"summary": "s"}}"#;
        let b = parse_brief_doc(json).unwrap();
        assert_eq!(b.summary, "s");
        let md = "# Summary\nmd summary";
        let b2 = parse_brief_doc(md).unwrap();
        assert_eq!(b2.summary, "md summary");
    }

    #[test]
    fn test_snippet_placeholder_collect_and_expand() {
        let doc = "before\n[SNIPPET id=\"3\"]\nmid\n[SNIPPET id=1] tail\n";
        let ids = collect_snippet_placeholder_ids(doc);
        assert_eq!(ids, vec![3, 1]);

        let mut blocks = std::collections::HashMap::new();
        blocks.insert(3, "--- src/foo.rs [12-40]\nfn foo() {}".to_string());
        blocks.insert(1, "--- src/bar.rs [1-4]\nlet x = 1;".to_string());
        let out = expand_snippet_placeholders(doc, &blocks);
        assert!(out.contains("--- src/foo.rs [12-40]\nfn foo() {}"), "must expand id=3");
        assert!(out.contains("--- src/bar.rs [1-4]\nlet x = 1;"), "must expand id=1");
        assert!(out.contains("before"), "must preserve text before placeholder");
        assert!(!out.contains("[SNIPPET"), "must leave no placeholders behind");

        // Unknown id becomes a visible note, not silent data loss.
        let out2 = expand_snippet_placeholders("[SNIPPET id=\"99\"]", &blocks);
        assert!(out2.contains("snippet id=99 not found"), "unknown id must be visible");

        // Malformed placeholder is preserved as-is.
        let out3 = expand_snippet_placeholders("[SNIPPET gibberish]", &blocks);
        assert!(out3.contains("[SNIPPET gibberish]"), "malformed placeholder kept");
    }

    #[test]
    fn test_parse_plan_review_approve_and_revise() {
        // Explicit approve.
        let (ok, notes) = parse_plan_review(&Some(r#"{"approved": true}"#.to_string()));
        assert!(ok);
        assert!(notes.is_none());

        // Revise with actionable notes.
        let (ok, notes) = parse_plan_review(&Some(
            r#"{"approved": false, "revision_notes": "- Task 2: add the anchor snippet\n- Task 3: split into two tasks"}"#.to_string(),
        ));
        assert!(!ok);
        assert!(notes.is_some());
        assert!(notes.as_deref().unwrap().contains("Task 2"));

        // Revise with NO notes cannot be applied → accept so the loop terminates.
        let (ok, notes) = parse_plan_review(&Some(r#"{"approved": false}"#.to_string()));
        assert!(ok);
        assert!(notes.is_none());

        // Missing / malformed args default to accept (guarantees termination).
        let (ok, notes) = parse_plan_review(&None);
        assert!(ok);
        assert!(notes.is_none());
        let (ok, _notes) = parse_plan_review(&Some("not json".to_string()));
        assert!(ok);
    }

    #[test]
    fn test_format_brief_digest_contains_sections() {
        let brief = ResearchBrief {
            summary: "do the thing".into(),
            key_files: vec![BriefFile {
                path: "src/lib.rs".into(),
                why: "entry".into(),
                key_symbols: vec!["main".into()],
            }],
            relevant_snippets: vec![],
            conventions: "use snake_case".into(),
            risks_unknowns: vec!["untested".into()],
            external_pulls: vec![],
        };
        let audit = ContextAudit {
            complete: true,
            summary: "all good".into(),
            missing: vec![],
        };
        let digest = format_brief_digest(&brief, &audit);
        assert!(digest.contains("do the thing"));
        assert!(digest.contains("src/lib.rs"));
        assert!(digest.contains("main"));
        assert!(digest.contains("use snake_case"));
        assert!(digest.contains("untested"));
        assert!(digest.contains("VERIFIER AUDIT"));
    }

    #[test]
    fn test_build_tree_diff_report_detects_change_and_creation() {
        let temp_dir = std::env::temp_dir().join("kuda_swarm_diff_test");
        let project_root = temp_dir.join("project");
        let app_data = temp_dir.join("app_data");
        let _ = std::fs::create_dir_all(&project_root);
        let _ = std::fs::create_dir_all(&app_data);

        let target = project_root.join("a.txt");
        let _ = std::fs::write(&target, "line1\nline2\n");

        let ctx = ToolContext {
            project_root: project_root.clone(),
            app_data_dir: app_data.clone(),
            external_requests: std::sync::Arc::new(
                crate::agent::tool_registry::ExternalRequestRegistry::new(),
            ),
            plan_decisions: std::sync::Arc::new(
                crate::agent::tool_registry::PlanDecisionRegistry::new(),
            ),
            direction_decisions: std::sync::Arc::new(
                crate::agent::tool_registry::DirectionDecisionRegistry::new(),
            ),
            session_id: None,
            cancel: crate::agent::tool_registry::CancelFlag::new(),
        };

        // Snapshot BEFORE the change.
        let before = snapshot_project_tree(&ctx);
        let _ = std::fs::write(&target, "line1\nline2 EDITED\nline3\n");
        let _ = std::fs::write(project_root.join("b.txt"), "brand new\n");

        let report = build_tree_diff_report(&ctx, &before);
        assert!(report.contains("a.txt"), "report: {}", report);
        assert!(report.contains("b.txt"), "report: {}", report);
        assert!(report.contains("EDITED") || report.contains("line3"), "report: {}", report);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_build_ledger_message_segments_and_truncation() {
        let ledger = TurnLedger {
            brief_digest: Some("b".repeat(3000)),
            plan_markdown: None,
            plan_status: Some("approved (review×1, revision×2)".into()),
            execution_review: Some("[VERDICT] PASSED".into()),
            final_answer: "answer".into(),
        };
        let msg = build_ledger_message(&ledger);
        assert_eq!(msg.name.as_deref(), Some("ledger"));
        assert!(msg.content.starts_with("[TURN LEDGER]"));
        assert!(msg.content.contains("[RESEARCH BRIEF]"));
        assert!(msg.content.contains("[PLAN STATUS] approved (review×1, revision×2)"));
        assert!(msg.content.contains("[EXECUTION REVIEW]"));
        assert!(msg.content.contains("[FINAL ANSWER]\nanswer"));
        // Empty plan segment renders as "—".
        assert!(msg.content.contains("[PLAN]\n—"));
        // Brief segment is capped at the ledger budget.
        let start = msg.content.find("[RESEARCH BRIEF]").unwrap();
        let end = msg.content.find("[PLAN]").unwrap();
        assert!(end - start <= LEDGER_BRIEF_CHARS + 40, "brief segment too large");
    }

    #[test]
    fn test_build_execution_review_ledger_verdicts() {
        let notes = vec!["[EXEC] Task #1 (code) by Executor Code — changed 1 file(s)".to_string()];
        let passed = build_execution_review_ledger(
            &notes,
            &Some(Verdict { passed: true, summary: "all ok".into(), issues: vec![] }),
            "",
        );
        assert!(passed.contains("[VERDICT] PASSED"));
        assert!(passed.contains("[EXEC] Task #1"));

        let failed = Verdict {
            passed: false,
            summary: "two bugs".into(),
            issues: vec![
                VerdictIssue { description: "i1".into(), kind: "code".into() },
                VerdictIssue { description: "i2".into(), kind: "design".into() },
                VerdictIssue { description: "i3".into(), kind: "code".into() },
                VerdictIssue { description: "i4 must be capped".into(), kind: "code".into() },
            ],
        };
        let fl = build_execution_review_ledger(&notes, &Some(failed), "");
        assert!(fl.contains("[VERDICT] FAILED"));
        assert_eq!(fl.matches("[ISSUE]").count(), 3);

        let un = build_execution_review_ledger(&notes, &None, "no verdict produced");
        assert!(un.contains("[VERDICT] UNVERIFIED"));
    }

    #[test]
    fn test_transcript_collector_phases_and_tool_calls() {
        let mut col = TranscriptCollector::new("run1".to_string());
        col.record(&AgentEventKind::PhaseStarted {
            role: "rlm_model".into(),
            label: "collect".into(),
            model: "m".into(),
        });
        col.record(&AgentEventKind::ThoughtDelta("hello".into()));
        col.record(&AgentEventKind::ToolCallStarted {
            call_id: "c1".into(),
            tool_name: "grep_search".into(),
            arguments_json: "{}".into(),
        });
        col.record(&AgentEventKind::ToolCallCompleted {
            call_id: "c1".into(),
            tool_name: "grep_search".into(),
            output: "x".repeat(2000),
        });
        col.record(&AgentEventKind::PhaseCompleted {
            role: "rlm_model".into(),
            summary: "done".into(),
            tokens_in: 5,
            tokens_out: 3,
            cached_in: 0,
        });
        col.record(&AgentEventKind::PhaseStarted {
            role: "thinker".into(),
            label: "plan".into(),
            model: "m2".into(),
        });
        col.record(&AgentEventKind::Finished {
            total_tokens_used: 10,
            tokens_in: 7,
            tokens_out: 3,
            cached_in: 0,
        });
        let records = col.finish();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].role, "rlm_model");
        assert_eq!(records[0].text, "hello");
        assert_eq!(records[0].summary, "done");
        assert_eq!(records[0].tool_calls.len(), 1);
        assert_eq!(records[0].tool_calls[0].status, "done");
        assert!(records[0].tool_calls[0].output.len() <= PHASE_TOOL_OUTPUT_CHARS + 20);
        assert_eq!(records[1].role, "thinker");
        assert!(col.finish().is_empty());
    }

    #[tokio::test]
    async fn test_plan_decision_registry_oneshot_resolve() {
        let reg = crate::agent::tool_registry::PlanDecisionRegistry::new();
        let rx = reg.register("p1");
        let ok = reg.resolve("p1", "revise".into(), Some("note".into()));
        assert!(ok);
        let (decision, note) = rx.await.unwrap();
        assert_eq!(decision, "revise");
        assert_eq!(note.as_deref(), Some("note"));
        assert!(!reg.resolve("p1", "execute".into(), None));
    }

    #[tokio::test]
    async fn test_direction_decision_registry_oneshot_resolve() {
        let reg = crate::agent::tool_registry::DirectionDecisionRegistry::new();
        let rx = reg.register("d1");
        let ok = reg.resolve("d1", "ubah".into(), Some("fokus ke halaman utama".into()));
        assert!(ok);
        let (decision, note) = rx.await.unwrap();
        assert_eq!(decision, "ubah");
        assert_eq!(note.as_deref(), Some("fokus ke halaman utama"));
        assert!(!reg.resolve("d1", "lanjut".into(), None));
    }

    #[test]
    fn test_resume_checkpoint_rejects_traversal_run_id() {
        let temp_dir = std::env::temp_dir().join("kuda_checkpoint_traversal_test");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();
        let cp = RunCheckpoint {
            phase: ResumePhase::Direction,
            shared: vec![],
            total_tokens: 0,
            tokens_in: 0,
            tokens_out: 0,
            cached_in: 0,
            ledger: TurnLedger {
                brief_digest: None,
                plan_markdown: None,
                plan_status: None,
                execution_review: None,
                final_answer: String::new(),
            },
            final_plan: None,
            pending_tasks: vec![],
            executor_logs: vec![],
            fix_round: 0,
            exec_notes: vec![],
        };

        // A hostile run_id must not escape resume_runs/ — write must be a
        // no-op, load must be None, and no file may appear in app-data.
        for evil in ["../hub_credentials", "../../evil", "/abs", "a/b"] {
            write_checkpoint(&temp_dir, evil, &cp);
            assert!(load_checkpoint(&temp_dir, evil).is_none());
            clear_checkpoint(&temp_dir, evil);
        }
        assert!(!temp_dir.join("hub_credentials.json").exists());
        assert!(!temp_dir.join("evil.json").exists());
        // Nothing was written at all: a rejected run_id must not even create
        // the resume_runs directory.
        assert!(std::fs::read_dir(&temp_dir).unwrap().next().is_none());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_resume_checkpoint_roundtrip() {
        let temp_dir = std::env::temp_dir().join("kuda_resume_checkpoint_test");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let run_id = "run_resume_1";
        let cp = RunCheckpoint {
            phase: ResumePhase::Executing,
            shared: vec![
                Message::user("[RESEARCH BRIEF — validated by RLM]\nsummary"),
                Message::user("[USER DIRECTION] Arah disetujui.\nbuat plan"),
            ],
            total_tokens: 123,
            tokens_in: 60,
            tokens_out: 40,
            cached_in: 23,
            ledger: TurnLedger {
                brief_digest: Some("digest".into()),
                plan_markdown: Some("plan".into()),
                plan_status: Some("approved".into()),
                execution_review: None,
                final_answer: String::new(),
            },
            final_plan: Some(SwarmPlan {
                goal: "g".into(),
                architecture: None,
                risks: None,
                tasks: vec![SwarmPlanTask {
                    id: 3,
                    kind: "code".into(),
                    description: "task3".into(),
                    context: None,
                    files: vec!["a.rs".into()],
                    acceptance: "works".into(),
                }],
            }),
            pending_tasks: vec![SwarmPlanTask {
                id: 3,
                kind: "code".into(),
                description: "task3".into(),
                context: None,
                files: vec![],
                acceptance: "works".into(),
            }],
            executor_logs: vec![Message::user("[EXECUTOR CONTEXT] task 1 done")],
            fix_round: 0,
            exec_notes: vec!["[EXEC] Task #1".into()],
        };
        write_checkpoint(&temp_dir, run_id, &cp);
        let loaded = load_checkpoint(&temp_dir, run_id).expect("checkpoint must load back");
        assert!(matches!(loaded.phase, ResumePhase::Executing));
        assert_eq!(loaded.total_tokens, 123);
        assert_eq!(loaded.tokens_in, 60);
        assert_eq!(loaded.tokens_out, 40);
        assert_eq!(loaded.cached_in, 23);
        assert_eq!(loaded.shared.len(), 2);
        assert_eq!(loaded.ledger.brief_digest.as_deref(), Some("digest"));
        assert_eq!(loaded.pending_tasks.len(), 1);
        assert_eq!(loaded.pending_tasks[0].id, 3);
        assert_eq!(loaded.executor_logs.len(), 1);
        assert_eq!(loaded.exec_notes.len(), 1);
        assert!(loaded.final_plan.is_some());
        clear_checkpoint(&temp_dir, run_id);
        assert!(load_checkpoint(&temp_dir, run_id).is_none());
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
