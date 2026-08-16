use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tauri::ipc::Channel;
use tokio::sync::{oneshot, Notify};
use crate::error::{AppError, Result};
use crate::file_system::io::FileSystemIO;
use crate::diff_engine::history::CheckpointManager;
use crate::agent::orchestrator::{AgentEvent, AgentEventKind};

/// Cooperative cancellation flag for one agent run. `cancel()` is called by the
/// `agent_cancel_run` command or when the UI event channel closes; the agent
/// loops check it between turns, chunks and tool calls.
#[derive(Clone)]
pub struct CancelFlag {
    inner: Arc<(AtomicBool, Notify)>,
}

impl CancelFlag {
    pub fn new() -> Self {
        Self {
            inner: Arc::new((AtomicBool::new(false), Notify::new())),
        }
    }

    pub fn cancel(&self) {
        self.inner.0.store(true, Ordering::SeqCst);
        // Wake every waiter already registered ...
        self.inner.1.notify_waiters();
        // ... AND store ONE permit so a waiter that subscribes between an
        // `is_cancelled()` check and `notified()` still wakes immediately
        // (closes the classic Notify race reported in the audit report).
        self.inner.1.notify_one();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.0.load(Ordering::SeqCst)
    }

    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.inner.1.notified()
    }

    /// Identity comparison (same underlying flag), used to remove exactly this
    /// run's entry from a shared `active_runs` bucket without touching sibling
    /// runs that happen to share the same run id.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Default for CancelFlag {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared registry of pending external-access requests plus the live event
/// channels. The `request_external_access` tool registers `oneshot` senders
/// here (one per requested path), emits `ExternalAccessRequest` events through
/// the channels, and awaits the receivers. The `agentApproveExternalAccess` /
/// `agentDenyExternalAccess` commands resolve the senders by `request_id`.
pub struct ExternalRequestRegistry {
    pending: std::sync::Mutex<HashMap<String, oneshot::Sender<bool>>>,
    /// Per-run streaming channels (one per active agent run). Events broadcast
    /// to ALL runs so concurrent agent runs never clobber each other's feed.
    run_channels: std::sync::Mutex<HashMap<u64, Channel<AgentEvent>>>,
    next_channel_id: AtomicU64,
    /// App-wide channel bound once by the frontend at startup. FS commands use
    /// it to surface Allow/Deny notifications even when no agent run is active.
    app_channel: std::sync::Mutex<Option<Channel<AgentEvent>>>,
}

impl ExternalRequestRegistry {
    pub fn new() -> Self {
        Self {
            pending: std::sync::Mutex::new(HashMap::new()),
            run_channels: std::sync::Mutex::new(HashMap::new()),
            next_channel_id: AtomicU64::new(1),
            app_channel: std::sync::Mutex::new(None),
        }
    }

    /// Registers the live streaming channel for one agent run and returns a
    /// token used to unregister it when the run ends. Returns a stable id so
    /// concurrent runs can be registered and removed independently.
    pub fn register_channel(&self, ch: Channel<AgentEvent>) -> u64 {
        let id = self.next_channel_id.fetch_add(1, Ordering::SeqCst);
        self.run_channels.lock().unwrap().insert(id, ch);
        id
    }

    pub fn unregister_channel(&self, id: u64) {
        self.run_channels.lock().unwrap().remove(&id);
    }

    /// Binds the persistent app-wide channel used by filesystem commands.
    pub fn bind_app(&self, ch: Channel<AgentEvent>) {
        *self.app_channel.lock().unwrap() = Some(ch);
    }

    pub fn emit(&self, kind: AgentEventKind) {
        for ch in self.run_channels.lock().unwrap().values() {
            let _ = ch.send(AgentEvent { kind: kind.clone() });
        }
        if let Some(ch) = self.app_channel.lock().unwrap().as_ref() {
            let _ = ch.send(AgentEvent { kind });
        }
    }

    /// Registers a pending request for an out-of-project path, emits an
    /// `ExternalAccessRequest` event, and awaits the user's Allow/Deny
    /// resolution. Returns `true` only when the user allowed the access.
    pub async fn request_approval(&self, path: &str, reason: &str, kind: &str) -> bool {
        let request_id = format!("ext_{}", uuid::Uuid::new_v4().simple());
        let rx = self.register(&request_id);
        self.emit(AgentEventKind::ExternalAccessRequest {
            request_id: request_id.clone(),
            path: path.to_string(),
            reason: reason.to_string(),
            kind: kind.to_string(),
        });

        let allowed = match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
            Ok(Ok(true)) => true,
            _ => false, // denied, sender dropped, or timed out
        };
        // Drop the stale registry entry on ANY path that did not go through
        // `resolve` (timeout, drop, denial-side drop) so a later approve call
        // cannot report "resolved" for a dead request.
        self.remove(&request_id);

        self.emit(AgentEventKind::ExternalAccessResolved {
            request_id,
            allowed,
        });
        allowed
    }

    /// Registers a pending request and returns the receiver the tool will await.
    pub fn register(&self, request_id: &str) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(request_id.to_string(), tx);
        rx
    }

    /// Resolves a pending request (called by the approve/deny commands).
    /// Returns true if a matching pending request existed.
    pub fn resolve(&self, request_id: &str, allowed: bool) -> bool {
        if let Some(sender) = self.pending.lock().unwrap().remove(request_id) {
            let _ = sender.send(allowed);
            true
        } else {
            false
        }
    }

    /// Removes a pending request WITHOUT resolving it. Used when a run
    /// times out / is cancelled mid-await, so the stale entry can never be
    /// resolved later (which would silently no-op) or leak forever.
    pub fn remove(&self, request_id: &str) -> bool {
        self.pending.lock().unwrap().remove(request_id).is_some()
    }

    /// Drops all pending requests (e.g. when a run aborts).
    pub fn cancel_all(&self) {
        self.pending.lock().unwrap().clear();
    }
}

/// Shared registry of pending plan-approval gate requests. The swarm gate
/// (after the Thinker produces a plan) registers a `oneshot` sender here,
/// emits a `PlanDecisionRequest` event, and awaits the receiver. The
/// `agent_resolve_plan_decision` command resolves the sender by `request_id`
/// with the user's decision ("execute" | "revise" | "review") plus an optional
/// revision note. Mirrors `ExternalRequestRegistry` exactly.
pub struct PlanDecisionRegistry {
    pending: std::sync::Mutex<HashMap<String, oneshot::Sender<(String, Option<String>)>>>,
}

impl PlanDecisionRegistry {
    pub fn new() -> Self {
        Self {
            pending: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn register(&self, request_id: &str) -> oneshot::Receiver<(String, Option<String>)> {
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(request_id.to_string(), tx);
        rx
    }

    /// Resolves a pending decision. Returns true if a matching request existed.
    pub fn resolve(&self, request_id: &str, decision: String, note: Option<String>) -> bool {
        if let Some(sender) = self.pending.lock().unwrap().remove(request_id) {
            let _ = sender.send((decision, note));
            true
        } else {
            false
        }
    }

    /// Removes a pending request without resolving it (timeout / cancel path).
    pub fn remove(&self, request_id: &str) -> bool {
        self.pending.lock().unwrap().remove(request_id).is_some()
    }

    /// Drops all pending decisions (e.g. when a run aborts or finishes).
    pub fn cancel_all(&self) {
        self.pending.lock().unwrap().clear();
    }
}

/// Shared registry of pending Thinker-direction checkpoint requests. The swarm
/// pauses after the Thinker writes its TEMPORARY CONCLUSION (before the full
/// plan) and awaits the user's "lanjut"/"ubah" decision via
/// `agent_resolve_direction_decision`. Mirrors `PlanDecisionRegistry` exactly.
pub struct DirectionDecisionRegistry {
    pending: std::sync::Mutex<HashMap<String, oneshot::Sender<(String, Option<String>)>>>,
}

impl DirectionDecisionRegistry {
    pub fn new() -> Self {
        Self {
            pending: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn register(&self, request_id: &str) -> oneshot::Receiver<(String, Option<String>)> {
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(request_id.to_string(), tx);
        rx
    }

    /// Resolves a pending direction decision. Returns true if a request existed.
    pub fn resolve(&self, request_id: &str, decision: String, note: Option<String>) -> bool {
        if let Some(sender) = self.pending.lock().unwrap().remove(request_id) {
            let _ = sender.send((decision, note));
            true
        } else {
            false
        }
    }

    /// Removes a pending request without resolving it (timeout / cancel path).
    pub fn remove(&self, request_id: &str) -> bool {
        self.pending.lock().unwrap().remove(request_id).is_some()
    }

    /// Drops all pending decisions (e.g. when a run aborts or finishes).
    pub fn cancel_all(&self) {
        self.pending.lock().unwrap().clear();
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
    pub requires_approval: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReplacementChunk {
    pub start_line: usize,
    pub end_line: usize,
    pub target_content: String,
    pub replacement_content: String,
}

#[derive(Clone)]
pub struct ToolContext {
    pub project_root: PathBuf,
    pub app_data_dir: PathBuf,
    pub external_requests: Arc<ExternalRequestRegistry>,
    /// Registry for the plan-approval gate (swarm human-in-the-loop).
    pub plan_decisions: Arc<PlanDecisionRegistry>,
    /// Registry for the Thinker-direction checkpoint (temp conclusion review
    /// before the full plan is created), resolved by
    /// `agent_resolve_direction_decision` ("lanjut" | "ubah").
    pub direction_decisions: Arc<DirectionDecisionRegistry>,
    /// Edit-session id for the current agent run. File mutations tag their
    /// checkpoints with it so the whole run can be reverted together.
    pub session_id: Option<String>,
    /// Cooperative cancellation for the current agent run.
    pub cancel: CancelFlag,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolResult {
    pub success: bool,
    pub output: String,
    pub is_error: bool,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult>;
}

pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            tools: HashMap::new(),
        };

        registry.register(Arc::new(BatchFileReadTool));
        registry.register(Arc::new(MultiReplaceFileTool));
        registry.register(Arc::new(ListDirTool));
        registry.register(Arc::new(RunCommandTool));
        registry.register(Arc::new(GrepSearchTool));
        registry.register(Arc::new(WriteFileTool));
        registry.register(Arc::new(SubmitPlanTool));
        registry.register(Arc::new(SubmitPlanReviewTool));
        registry.register(Arc::new(SubmitReviewDirectionsTool));
        registry.register(Arc::new(SubmitVerdictTool));
        registry.register(Arc::new(SubmitAuditTool));
        registry.register(Arc::new(SubmitBriefTool));
        registry.register(Arc::new(RequestRlmResearchTool));
        registry.register(Arc::new(RequestExternalAccessTool));
        registry.register(Arc::new(crate::agent::rlm_kernel::RlmPythonTool));
        registry.register(Arc::new(CodeOutlineTool));

        registry
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let def = tool.definition();
        self.tools.insert(def.name.clone(), tool);
    }

    pub fn get_definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    /// Returns only the tool definitions whose names are in the allowed list,
    /// preserving a stable ordering (definition registration order is HashMap-based,
    /// so we order by the allowed list instead for prompt-cache-stable output).
    pub fn get_definitions_filtered(&self, allowed: &[String]) -> Vec<ToolDefinition> {
        allowed
            .iter()
            .filter_map(|name| self.tools.get(name))
            .map(|t| t.definition())
            .collect()
    }

    pub async fn execute_tool(&self, name: &str, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| AppError::General(format!("Tool '{}' not registered", name)))?;

        tool.execute(params, ctx).await
    }
}

/// Byte offset of the start of every line in `content` (0-based line index).
fn line_byte_starts(content: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (idx, ch) in content.char_indices() {
        if ch == '\n' {
            starts.push(idx + 1);
        }
    }
    starts
}

/// Byte offset where the chunk's `start_line` begins in `content`. Used only to
/// order chunks bottom-up so earlier edits never shift later chunks' line numbers.
fn range_start_byte(chunk: &ReplacementChunk, content: &str) -> usize {
    let line_starts = line_byte_starts(content);
    let total = line_starts.len();
    let first = chunk.start_line.saturating_sub(1).min(total.saturating_sub(1));
    line_starts.get(first).copied().unwrap_or(0)
}

/// Resolves a (possibly relative) tool path against the project root so tool
/// behaviour never depends on the process CWD.
fn resolve_path(project_root: &Path, p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        pb
    } else {
        project_root.join(pb)
    }
}

/// Destinations `rm -r` must never target: the filesystem root, home (direct
/// or via shell expansion), the current/parent dir, glob-everything, or a
/// system tree. Matching is done on trailing `/` and `*` stripped, against
/// the lowercased token (`$HOME` arrives here as `$home`).
const BROAD_RM_TARGETS: &[&str] = &[
    "/", "/etc", "/usr", "/var", "/boot", "/dev", "/sys", "/proc", "/bin", "/sbin", "/lib",
    "/lib64", "/opt", "/private", "/system", "/library", "/applications", "/users", "/home",
    "/tmp", "/srv", "/run", "/windows", "/program files", "~", "$home", "${home}", ".", "..",
    "*", ".*",
];

fn is_broad_rm_target(token: &str) -> bool {
    let t = token.trim_matches(|c| c == '"' || c == '\'');
    let t = t.strip_suffix('*').unwrap_or(t);
    // Keep a bare "/" intact — stripping its trailing slash would turn it
    // into an empty string.
    let t = if t.chars().count() > 1 {
        t.strip_suffix('/').unwrap_or(t)
    } else {
        t
    };
    if BROAD_RM_TARGETS.contains(&t) {
        return true;
    }
    // The user's REAL home path (e.g. /Users/macmini) — deleting the whole
    // home directory is catastrophic even though it is not a system tree.
    if let Some(home) = std::env::var_os("HOME") {
        if t == home.to_string_lossy().to_lowercase() {
            return true;
        }
    }
    false
}

/// Guards the `run_command` tool against obviously destructive commands. This
/// is a secondary safety net on top of the interactive approval gate.
///
/// Token-based analysis (not substring matching) so trivial obfuscations do
/// not slip through: `rm -rf ~`, `rm -r $HOME`, `rm -fr .`, `rm -rf /home`
/// and fork bombs (`:(){ :|:& };:`, `f(){ f|f& }; f`) are all blocked.
fn is_destructive_command(cmd: &str) -> bool {
    let c = cmd.trim().to_lowercase();

    // Fork bombs: canonical `:(){ :|:& };:` and renamed/respaced variants all
    // compact to `x(){...|...&...};...` — match `(){` + `&};`.
    let compact: String = c.chars().filter(|ch| !ch.is_whitespace()).collect();
    if compact.contains("(){") && compact.contains("&};") {
        return true;
    }

    // Raw device / filesystem killers (kept from the original guard).
    if c.contains("mkfs")
        || c.contains("dd if=")
        || c.contains("of=/dev/")
        || c.contains("> /dev/sd")
        || c.contains("shred /")
        || c.contains("shred /dev/")
    {
        return true;
    }

    // rm with a recursive flag aimed at a broad target. Scan every token so
    // `sudo rm -rf ~`, `rm --recursive -f /home`, `rm -fr "$HOME"` all match.
    // Options are order-independent for /bin/rm, so a recursive flag appearing
    // AFTER the target (`rm -f /etc -r`, `rm -f ~ -R`) must still flag the
    // target — the previous scanner only checked targets while `recursive` was
    // already set, which let `-f /etc -r` slip through.
    let tokens: Vec<String> = c
        .split_whitespace()
        .map(|t| t.trim_matches(|ch| ch == '"' || ch == '\'').to_string())
        .collect();
    for (i, tok) in tokens.iter().enumerate() {
        if tok != "rm" {
            continue;
        }
        let rest: Vec<&String> = tokens.iter().skip(i + 1).collect();
        // Any recursive flag anywhere after `rm` (before `--`) makes the whole
        // invocation recursive; `--` switches everything after it to targets.
        let recursive = rest.iter().any(|t| {
            if *t == "--" {
                return false;
            }
            if *t == "--no-preserve-root" || *t == "--recursive" || *t == "-r" || *t == "-R" {
                return true;
            }
            t.starts_with('-') && !t.starts_with("--") && t.chars().skip(1).any(|ch| ch == 'r' || ch == 'R')
        });
        if !recursive {
            continue;
        }
        for t in rest {
            if t.starts_with('-') {
                continue; // flags are not targets (short/long options skipped)
            }
            if is_broad_rm_target(t) {
                return true;
            }
        }
    }
    false
}

// 1. Batch Multi-File Reading Tool
pub struct BatchFileReadTool;

#[async_trait]
impl Tool for BatchFileReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "batch_file_read".to_string(),
            description: "Reads multiple files at once in a single turn. Supports optional start_line & end_line or pattern filter.".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Array of file paths to read"
                    },
                    "start_line": { "type": "integer", "description": "Optional start line (1-indexed)" },
                    "end_line": { "type": "integer", "description": "Optional end line (1-indexed)" },
                    "pattern": { "type": "string", "description": "Optional pattern substring to filter matching lines only" }
                },
                "required": ["paths"]
            }),
            requires_approval: false,
        }
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let paths: Vec<String> = serde_json::from_value(params.get("paths").cloned().unwrap_or_default())
            .map_err(|e| AppError::General(format!("Invalid paths param: {}", e)))?;
        
        let start_line = params.get("start_line").and_then(|v| v.as_u64()).map(|v| v as usize);
        let end_line = params.get("end_line").and_then(|v| v.as_u64()).map(|v| v as usize);
        let pattern_opt = params.get("pattern").and_then(|v| v.as_str());

        let mut output = String::new();
        let mut had_error = false;
        for p in paths {
            let path_buf = resolve_path(&ctx.project_root, &p);
            match FileSystemIO::read_file(&path_buf, &ctx.project_root, start_line, end_line) {
                Ok(content) => {
                    let lines: Vec<&str> = content.content.lines().collect();
                    let total_lines = lines.len();
                    output.push_str(&format!("=== FILE: {} [total_lines: {}] ===\n", p, total_lines));

                    if let Some(pat) = pattern_opt {
                        let pat_lower = pat.to_lowercase();
                        let mut match_count = 0;
                        for (idx, line) in lines.iter().enumerate() {
                            let line_num = start_line.unwrap_or(1) + idx;
                            if line.to_lowercase().contains(&pat_lower) {
                                output.push_str(&format!("{}: {}\n", line_num, line));
                                match_count += 1;
                            }
                        }
                        if match_count == 0 {
                            output.push_str(&format!("(No lines matching pattern '{}')\n", pat));
                        }
                        output.push('\n');
                    } else {
                        output.push_str(&content.content);
                        output.push_str("\n\n");
                    }
                }
                Err(e) => {
                    had_error = true;
                    output.push_str(&format!("=== ERROR READING {}: {} ===\n\n", p, e));
                }
            }
        }

        Ok(ToolResult {
            success: true,
            output,
            // A failed read is surfaced as an error signal so the model knows
            // the listed output is partial (previously the tool reported
            // success even when every file failed to read).
            is_error: had_error,
        })
    }
}

// 2. Multi-Replace File Tool (Non-contiguous Multi-Chunk Editing)
pub struct MultiReplaceFileTool;

#[async_trait]
impl Tool for MultiReplaceFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "multi_replace_file".to_string(),
            description: "Applies multiple non-contiguous surgical replacement chunks to a file in a single response turn. Automatically creates a Full File Checkpoint before editing.".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target_file": { "type": "string", "description": "Target file path" },
                    "replacement_chunks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "start_line": { "type": "integer" },
                                "end_line": { "type": "integer" },
                                "target_content": { "type": "string" },
                                "replacement_content": { "type": "string" }
                            },
                            "required": ["start_line", "end_line", "target_content", "replacement_content"]
                        }
                    }
                },
                "required": ["target_file", "replacement_chunks"]
            }),
            requires_approval: true,
        }
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let target_file_str = params.get("target_file").and_then(|v| v.as_str()).unwrap_or("");
        let chunks: Vec<ReplacementChunk> = serde_json::from_value(params.get("replacement_chunks").cloned().unwrap_or_default())
            .map_err(|e| AppError::General(format!("Invalid chunks: {}", e)))?;

        let target_path = resolve_path(&ctx.project_root, target_file_str);
        let chk_mgr = CheckpointManager::new(&ctx.app_data_dir)?;

        // 1. Read current full file
        let current_payload = FileSystemIO::read_file(&target_path, &ctx.project_root, None, None)?;
        let mut content = current_payload.content;

        // 1b. Validate every chunk's line range BEFORE any I/O or overlap
        // analysis: lines are 1-indexed, so 0 anywhere is invalid. Checking it
        // here (instead of skipping such chunks in the overlap loop and
        // failing later in the apply loop) rejects the whole request up front.
        if let Some(chunk) = chunks
            .iter()
            .find(|c| c.start_line == 0 || c.end_line == 0)
        {
            return Ok(ToolResult {
                success: false,
                output: format!(
                    "Invalid chunk line range {}-{}: lines are 1-indexed.",
                    chunk.start_line, chunk.end_line
                ),
                is_error: true,
            });
        }

        // 1c. Reject overlapping chunk line ranges BEFORE applying. Bottom-up
        // application only works when the chunks are disjoint: an overlapping
        // chunk would rewrite content that the other chunk's declared range
        // also covers, so the upper chunk's target_content would no longer be
        // found and the edit fails midway.
        let mut sorted_by_start: Vec<&ReplacementChunk> = chunks.iter().collect();
        sorted_by_start.sort_by_key(|c| c.start_line);
        let mut prev_end = 0usize;
        for c in &sorted_by_start {
            if c.start_line <= prev_end {
                return Ok(ToolResult {
                    success: false,
                    output: format!(
                        "Overlapping replacement chunks: a chunk starts at line {} but the \
                         previous chunk already covers up to line {}. Chunks must be disjoint.",
                        c.start_line, prev_end
                    ),
                    is_error: true,
                });
            }
            prev_end = c.end_line;
        }

        // 2. Apply replacement chunks scoped to their declared line ranges.
        //    Chunks are applied bottom-up (highest line range first) so an edit
        //    below never shifts the line numbers of the chunks above it.
        let mut indexed: Vec<(usize, &ReplacementChunk)> = chunks.iter().enumerate().collect();
        indexed.sort_by(|a, b| {
            let ra = range_start_byte(a.1, &content);
            let rb = range_start_byte(b.1, &content);
            rb.cmp(&ra)
        });

        let mut applied = 0usize;
        for (_idx, chunk) in indexed {
            if chunk.target_content.is_empty() {
                return Ok(ToolResult {
                    success: false,
                    output: "Empty target_content chunk rejected.".to_string(),
                    is_error: true,
                });
            }
            let line_starts = line_byte_starts(&content);
            let total_lines = line_starts.len();
            // The declared range must actually exist in the file: silently
            // clamping an out-of-range chunk used to edit the WRONG lines
            // (e.g. a 500-600 chunk on a 20-line file clamped to line 20 and
            // matched text elsewhere). Reject loudly instead.
            if chunk.end_line > total_lines || chunk.start_line > total_lines {
                return Ok(ToolResult {
                    success: false,
                    output: format!(
                        "Chunk line range {}-{} exceeds the file's {} line(s).",
                        chunk.start_line, chunk.end_line, total_lines
                    ),
                    is_error: true,
                });
            }
            let first = chunk.start_line - 1;
            let last = chunk.end_line - 1;
            if first > last {
                return Ok(ToolResult {
                    success: false,
                    output: format!(
                        "Invalid chunk line range {}-{}: start_line must be <= end_line.",
                        chunk.start_line, chunk.end_line
                    ),
                    is_error: true,
                });
            }
            let range_start = line_starts[first];
            let range_end = line_starts.get(last + 1).copied().unwrap_or(content.len());
            let range = &content[range_start..range_end];
            let Some(rel) = range.find(chunk.target_content.as_str()) else {
                return Ok(ToolResult {
                    success: false,
                    output: format!(
                        "Target content chunk at line {}-{} not found within that line range.",
                        chunk.start_line, chunk.end_line
                    ),
                    is_error: true,
                });
            };
            let abs = range_start + rel;
            content = format!(
                "{}{}{}",
                &content[..abs],
                chunk.replacement_content,
                &content[abs + chunk.target_content.len()..]
            );
            applied += 1;
        }

        // 3. Save modified content with AUTOMATIC FULL FILE CHECKPOINT (session-tagged)
        let chk = FileSystemIO::write_file_in_session(
            &target_path,
            &content,
            &ctx.project_root,
            &chk_mgr,
            None,
            ctx.session_id.clone(),
        )?;

        let chk_id = chk.map(|c| c.checkpoint_id).unwrap_or_else(|| "N/A".to_string());
        Ok(ToolResult {
            success: true,
            output: format!("Applied {} surgical chunks to {}. Full File Checkpoint ID: {}", applied, target_file_str, chk_id),
            is_error: false,
        })
    }
}

// 3. List Directory Tool
pub struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "list_dir".to_string(),
            description: "Lists all files and subdirectories inside a directory.".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "dir_path": { "type": "string", "description": "Directory path" }
                },
                "required": ["dir_path"]
            }),
            requires_approval: false,
        }
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let dir_str = params.get("dir_path").and_then(|v| v.as_str()).unwrap_or(".");
        let dir_path = resolve_path(&ctx.project_root, dir_str);

        match FileSystemIO::list_dir(&dir_path, &ctx.project_root) {
            Ok(entries) => {
                let json = serde_json::to_string_pretty(&entries)?;
                Ok(ToolResult {
                    success: true,
                    output: json,
                    is_error: false,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: format!("Failed to list dir: {}", e),
                is_error: true,
            }),
        }
    }
}

// 4. Run Command Tool (Terminal Shell Execution & Waiting)
pub struct RunCommandTool;

#[async_trait]
impl Tool for RunCommandTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "run_command".to_string(),
            description: "Executes a terminal shell command (e.g. 'python script.py', 'npm test', 'cargo build', 'git status') inside the project directory, waits for completion, and returns stdout & stderr output.".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Exact command line string to execute in shell"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory relative to project root"
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Optional execution timeout in seconds (default: 60)"
                    }
                },
                "required": ["command"]
            }),
            requires_approval: true,
        }
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let cmd_str = params.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if cmd_str.trim().is_empty() {
            return Ok(ToolResult {
                success: false,
                output: "Command string cannot be empty".to_string(),
                is_error: true,
            });
        }

        if is_destructive_command(cmd_str) {
            return Ok(ToolResult {
                success: false,
                output: "Command blocked by the Kuda safety guard: it targets critical system files (e.g. rm -rf /, mkfs, dd to a device). Use a narrower command scoped to the project.".to_string(),
                is_error: true,
            });
        }

        let rel_cwd = params.get("cwd").and_then(|v| v.as_str()).unwrap_or(".");
        // The working directory must stay inside the project: `PathBuf::join`
        // silently discards the root for ABSOLUTE `cwd` values (and `..` can
        // climb out), which would run the command outside the project tree.
        let working_dir = if rel_cwd == "." || rel_cwd.is_empty() {
            ctx.project_root.clone()
        } else {
            let candidate = PathBuf::from(rel_cwd);
            if candidate.is_absolute() {
                return Ok(ToolResult {
                    success: false,
                    output: format!(
                        "Invalid cwd '{}': must be RELATIVE to the project root.",
                        rel_cwd
                    ),
                    is_error: true,
                });
            }
            match crate::security::PathGuard::validate_path_in_scope(
                ctx.project_root.join(candidate),
                &ctx.project_root,
            ) {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: format!("Invalid cwd '{}': {}", rel_cwd, e),
                        is_error: true,
                    })
                }
            }
        };

        let timeout_secs = params
            .get("timeout_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(60);

        #[cfg(target_os = "windows")]
        let mut cmd = tokio::process::Command::new("cmd");
        #[cfg(target_os = "windows")]
        cmd.args(["/C", cmd_str]);

        #[cfg(not(target_os = "windows"))]
        let mut cmd = tokio::process::Command::new("sh");
        #[cfg(not(target_os = "windows"))]
        cmd.args(["-c", cmd_str]);

        cmd.current_dir(&working_dir);
        // Prevent orphaned child processes: if the command outlives the timeout,
        // the `output()` future is dropped while the shell is still running, and
        // tokio would leave it as a ghost process (dev servers, hanging scripts).
        cmd.kill_on_drop(true);

        let output_fut = cmd.output();

        // Cancellable execution: `agent_cancel_run` must interrupt a running
        // shell command instead of waiting out its timeout. On cancel the
        // `output()` future is dropped and `kill_on_drop(true)` reaps the child.
        let run = tokio::select! {
            biased;
            _ = ctx.cancel.notified() => None,
            res = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), output_fut) => Some(res),
        };

        match run {
            None => Ok(ToolResult {
                success: false,
                output: "Command cancelled by user.".to_string(),
                is_error: true,
            }),
            Some(Ok(Ok(out))) => {
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                let status_code = out.status.code().unwrap_or(-1);
                let success = out.status.success();

                let mut combined_output = String::new();
                if !stdout.is_empty() {
                    combined_output.push_str("=== STDOUT ===\n");
                    combined_output.push_str(&stdout);
                    combined_output.push('\n');
                }
                if !stderr.is_empty() {
                    combined_output.push_str("=== STDERR ===\n");
                    combined_output.push_str(&stderr);
                    combined_output.push('\n');
                }
                combined_output.push_str(&format!("=== EXIT CODE: {} ===", status_code));

                Ok(ToolResult {
                    success,
                    output: combined_output,
                    is_error: !success,
                })
            }
            Some(Ok(Err(e))) => Ok(ToolResult {
                success: false,
                output: format!("Failed to launch process: {}", e),
                is_error: true,
            }),
            Some(Err(_)) => Ok(ToolResult {
                success: false,
                output: format!("Command timed out after {} seconds", timeout_secs),
                is_error: true,
            }),
        }
    }
}

// 5. Grep Search Tool (Ripgrep-backed content search for Thinker/Reviewer research)
pub struct GrepSearchTool;

#[async_trait]
impl Tool for GrepSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep_search".to_string(),
            description: "Regex content search across the whole project (ripgrep-backed, respects .gitignore). Returns matching file paths with line numbers and line content, or set files_only: true to return only matching file paths.".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex or literal search pattern" },
                    "max_results": { "type": "integer", "description": "Optional max number of matches (default 100)" },
                    "files_only": { "type": "boolean", "description": "If true, returns only matching file paths without line numbers or content (token efficient)" },
                    "case_sensitive": { "type": "boolean", "description": "If true, match case-sensitively (default false)" }
                },
                "required": ["pattern"]
            }),
            requires_approval: false,
        }
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let pattern = params.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
        if pattern.trim().is_empty() {
            return Ok(ToolResult {
                success: false,
                output: "Search pattern cannot be empty".to_string(),
                is_error: true,
            });
        }
        let max_results = params
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(100);

        let files_only = params
            .get("files_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let case_sensitive = params
            .get("case_sensitive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let query = crate::indexer::search::SearchQuery {
            pattern: pattern.to_string(),
            is_regex: true,
            // Honor the flag (the backend used to ignore it and always search
            // case-sensitively).
            case_sensitive,
            max_results: Some(max_results),
        };

        match crate::indexer::search::CodeSearcher::search(&query, &ctx.project_root) {
            Ok(matches) => {
                let mut output = String::new();
                if files_only {
                    let mut unique_files: Vec<String> = Vec::new();
                    for m in &matches {
                        let rel = m
                            .file_path
                            .strip_prefix(&ctx.project_root)
                            .unwrap_or(&m.file_path);
                        let rel_str = rel.to_string_lossy().to_string();
                        if !unique_files.contains(&rel_str) {
                            unique_files.push(rel_str);
                        }
                    }
                    for f in unique_files {
                        output.push_str(&format!("{}\n", f));
                    }
                } else {
                    for m in &matches {
                        let rel = m
                            .file_path
                            .strip_prefix(&ctx.project_root)
                            .unwrap_or(&m.file_path);
                        output.push_str(&format!(
                            "{}:{}: {}\n",
                            rel.to_string_lossy(),
                            m.line_number,
                            m.line_content
                        ));
                    }
                }
                if matches.is_empty() {
                    output = "No matches found.".to_string();
                }
                Ok(ToolResult {
                    success: true,
                    output,
                    is_error: false,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: format!("Search failed: {}", e),
                is_error: true,
            }),
        }
    }
}

// 6. Write File Tool (create new file or full overwrite, with automatic checkpoint)
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".to_string(),
            description: "Creates a new file or fully overwrites an existing file with the given content. Creates parent directories when needed and an automatic Full File Checkpoint when the file already exists.".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Target file path (relative to project root)" },
                    "content": { "type": "string", "description": "Full file content to write" }
                },
                "required": ["path", "content"]
            }),
            requires_approval: true,
        }
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let path_str = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let content = params.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if path_str.trim().is_empty() {
            return Ok(ToolResult {
                success: false,
                output: "Path cannot be empty".to_string(),
                is_error: true,
            });
        }

        let target_path = resolve_path(&ctx.project_root, path_str);

        // Ensure parent directories exist (still validated inside PathGuard via write_file)
        if let Some(parent) = target_path.parent() {
            if !parent.as_os_str().is_empty() {
                let canonical_root = crate::security::PathGuard::validate_path_in_scope(&ctx.project_root, &ctx.project_root)?;
                let canonical_parent = crate::security::PathGuard::validate_path_in_scope(parent, &ctx.project_root)?;
                if canonical_parent.starts_with(&canonical_root) {
                    std::fs::create_dir_all(&canonical_parent)?;
                }
            }
        }

        let chk_mgr = CheckpointManager::new(&ctx.app_data_dir)?;
        match FileSystemIO::write_file_in_session(
            &target_path,
            content,
            &ctx.project_root,
            &chk_mgr,
            None,
            ctx.session_id.clone(),
        ) {
            Ok(chk) => {
                let chk_id = chk.map(|c| c.checkpoint_id).unwrap_or_else(|| "N/A (new file)".to_string());
                Ok(ToolResult {
                    success: true,
                    output: format!("Wrote {} bytes to {}. Checkpoint: {}", content.len(), path_str, chk_id),
                    is_error: false,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: format!("Write failed: {}", e),
                is_error: true,
            }),
        }
    }
}

// 7. Submit Plan Tool (handoff: Thinker/Reviewer -> Orchestrator)
// The model writes the COMPLETE plan to the project file via write_file, then
// calls this tool with only a tiny file path — never the plan body, which is
// far too large to fit in tool-call arguments (that caused JSON truncation)
// and which must NOT be pasted in the response text (it would flood the UI).
pub struct SubmitPlanTool;

#[async_trait]
impl Tool for SubmitPlanTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "submit_plan".to_string(),
            description: "Submits the final execution plan. FIRST write the COMPLETE plan markdown to the project file (e.g. \".kuda/plan.md\") using write_file, THEN call this tool exactly once with that project-relative file path. Do NOT put the plan body in this call's arguments, and do NOT paste the plan in your response text — write a short conclusion instead.".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Project-relative path where the plan markdown is stored (e.g. \".kuda/plan.md\")"
                    }
                },
                "required": ["file_path"]
            }),
            requires_approval: false,
        }
    }

    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        // The swarm orchestrator intercepts this call before execution.
        Ok(ToolResult {
            success: true,
            output: "Plan submitted.".to_string(),
            is_error: false,
        })
    }
}

// 7b. Submit Plan Review Tool (handoff: Thinker review -> Orchestrator).
// Used inside the Planning Writer loop: the (expensive) Thinker READS the
// Planning Writer's draft plan and emits ONLY a short decision here — whether
// the draft matches its idea (approved) or which specific corrections the
// writer must make (revision_notes). Keeping the verdict in this tiny tool call
// (instead of rewriting the whole plan) is what makes the cheap-writer /
// expensive-reader split save output tokens.
pub struct SubmitPlanReviewTool;

#[async_trait]
impl Tool for SubmitPlanReviewTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "submit_plan_review".to_string(),
            description: "Submits your review of the Planning Writer's draft plan. Call this exactly once: set \"approved\" to true when the draft matches your intended design, OR set \"approved\" to false and write the SPECIFIC corrections the writer must make in \"revision_notes\" (one bullet per issue, naming the exact task/section and what is wrong or missing). Never rewrite the plan yourself — the Planning Writer applies your notes.".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "approved": {
                        "type": "boolean",
                        "description": "true when the draft plan matches your intended design; false when it needs corrections"
                    },
                    "revision_notes": {
                        "type": "string",
                        "description": "When NOT approved, the specific corrections the Planning Writer must make. One bullet per issue, naming the exact task/section and what is wrong or missing. Leave empty when approved."
                    }
                },
                "required": ["approved"]
            }),
            requires_approval: false,
        }
    }

    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        // The swarm orchestrator intercepts this call before execution.
        Ok(ToolResult {
            success: true,
            output: "Plan review submitted.".to_string(),
            is_error: false,
        })
    }
}
pub struct SubmitVerdictTool;

#[async_trait]
impl Tool for SubmitVerdictTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "submit_verdict".to_string(),
            description: "Submits the final verification verdict. FIRST write the complete verdict as markdown in your response text (use the verdict template from your system prompt), THEN call this tool exactly once with the project-relative file path where the verdict is stored (e.g. \".kuda/verdict.md\"). Do NOT put the verdict body in this call's arguments — keep them tiny.".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Project-relative path where the verdict markdown is stored (e.g. \".kuda/verdict.md\")"
                    }
                },
                "required": ["file_path"]
            }),
            requires_approval: false,
        }
    }

    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        // The swarm orchestrator intercepts this call before execution.
        Ok(ToolResult {
            success: true,
            output: "Verdict submitted.".to_string(),
            is_error: false,
        })
    }
}

// 9. Submit Review Directions Tool (handoff: Reviewer utama -> Thinker -> Orchestrator)
pub struct SubmitReviewDirectionsTool;

#[async_trait]
impl Tool for SubmitReviewDirectionsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "submit_review_directions".to_string(),
            description: "Submits your review of the completed plan. You are a read-only auditor: you NEVER write files and NEVER rewrite the plan. Audit the plan for BUGS, LOGIC ERRORS, flawed assumptions, missing tasks, and things that could be IMPROVED to make the result more detailed and robust. Then call this tool exactly once: set \"approved\" to true when the plan is solid, OR set \"approved\" to false and list each required change as one item in \"directions\" (name the exact task/section, what is wrong, and what must change). The Thinker receives your directions and decides how to revise.".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "approved": {
                        "type": "boolean",
                        "description": "true when the plan is solid; false when changes are needed"
                    },
                    "directions": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "When NOT approved: one concrete direction per issue — exact task/section, what is wrong, what must change. Empty when approved."
                    }
                },
                "required": ["approved"]
            }),
            requires_approval: false,
        }
    }

    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        // The swarm orchestrator intercepts this call before execution.
        Ok(ToolResult {
            success: true,
            output: "Review directions submitted.".to_string(),
            is_error: false,
        })
    }
}

// 10. Submit Context Audit Tool (handoff: Context Guard -> Orchestrator)
pub struct SubmitAuditTool;

#[async_trait]
impl Tool for SubmitAuditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "submit_audit".to_string(),
            description: "Submits the context-completeness audit result. FIRST write the complete audit as markdown in your response text (use the audit template from your system prompt), THEN call this tool exactly once with the project-relative file path where the audit is stored (e.g. \".kuda/audit.md\"). Do NOT put the audit body in this call's arguments — keep them tiny.".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Project-relative path where the audit markdown is stored (e.g. \".kuda/audit.md\")"
                    }
                },
                "required": ["file_path"]
            }),
            requires_approval: false,
        }
    }

    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        // The swarm orchestrator intercepts this call before execution.
        Ok(ToolResult {
            success: true,
            output: "Audit submitted.".to_string(),
            is_error: false,
        })
    }
}

// 10. Submit Brief Tool (handoff: RLM Model -> Orchestrator/Thinker)
pub struct SubmitBriefTool;

#[async_trait]
impl Tool for SubmitBriefTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "submit_brief".to_string(),
            description: "Submits the validated research brief. FIRST write the complete brief as markdown in your response text (use the brief template from your system prompt), THEN call this tool exactly once with the project-relative file path where the brief is stored (e.g. \".kuda/brief.md\"). Do NOT put the brief body in this call's arguments — keep them tiny.".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Project-relative path where the brief markdown is stored (e.g. \".kuda/brief.md\")"
                    }
                },
                "required": ["file_path"]
            }),
            requires_approval: false,
        }
    }

    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        // The swarm orchestrator intercepts this call before execution.
        Ok(ToolResult {
            success: true,
            output: "Brief submitted.".to_string(),
            is_error: false,
        })
    }
}

// 11. Request RLM Research Tool (handoff: Thinker -> RLM Model -> Orchestrator)
pub struct RequestRlmResearchTool;

#[async_trait]
impl Tool for RequestRlmResearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "request_rlm_research".to_string(),
            description: "Requests the RLM researcher to collect ADDITIONAL data not covered by the validated brief. The RLM agent re-runs with its full research tools (kernel, grep, code_outline, batch_file_read, run_command) and reports back a compact supplement that is appended to your context. Use ONLY when the plan needs a specific fact the brief lacks (e.g. an API shape, a config value, build/test output). Ask at most twice per run; anything you can defer goes into Risks/Unknowns. Keep the arguments tiny and precise.".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "topic": {
                        "type": "string",
                        "description": "One-line description of what to research and why"
                    },
                    "questions": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Concrete questions the RLM must answer, one per item"
                    },
                    "scope_hints": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional file paths / symbols / commands that point to where the answer lives"
                    }
                },
                "required": ["topic", "questions"]
            }),
            requires_approval: false,
        }
    }

    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        // The swarm orchestrator intercepts this call before execution.
        Ok(ToolResult {
            success: true,
            output: "Research requested from RLM.".to_string(),
            is_error: false,
        })
    }
}

// 12. Request External Access Tool (interactive allowlist prompt for out-of-project reads)
pub struct RequestExternalAccessTool;

#[async_trait]
impl Tool for RequestExternalAccessTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "request_external_access".to_string(),
            description: "Asks the user for permission to read files/directories OUTSIDE the project root (e.g. system config, installed dependencies, parent repos). The RLM kernel blocks all out-of-project reads until the user approves them via this tool. Returns which paths were allowed/denied; re-issue your read after approval.".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "requests": {
                        "type": "array",
                        "description": "Out-of-project paths to ask permission for",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string", "description": "Canonical absolute path the model wants to read" },
                                "reason": { "type": "string", "description": "Why this path is relevant to the request" },
                                "kind": { "type": "string", "enum": ["file_read", "dir_scan"], "description": "Whether a single file or a directory scan is needed" }
                            },
                            "required": ["path", "reason", "kind"]
                        }
                    }
                },
                "required": ["requests"]
            }),
            requires_approval: false,
        }
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let requests = params.get("requests").cloned().unwrap_or(Value::Array(vec![]));
        let req_arr: Vec<Value> = serde_json::from_value(requests)
            .map_err(|e| AppError::General(format!("Invalid requests param: {}", e)))?;

        if req_arr.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: "No external paths requested.".to_string(),
                is_error: true,
            });
        }

        // One oneshot receiver per requested path; emit one event per path.
        let mut receivers: Vec<(String, String, oneshot::Receiver<bool>)> = Vec::new();
        let mut invalid_paths: Vec<String> = Vec::new();
        for req in &req_arr {
            let path = req.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let reason = req.get("reason").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let kind = req.get("kind").and_then(|v| v.as_str()).unwrap_or("file_read").to_string();
            if path.trim().is_empty() {
                continue;
            }
            // The allowlist is resolved against absolute paths (canonicalized by
            // `add_allowed_root`). A RELATIVE path would be canonicalized against
            // the process CWD, allowlisting an unintended location — reject it.
            if !PathBuf::from(&path).is_absolute() {
                invalid_paths.push(format!("{} (not an absolute path)", path));
                continue;
            }
            let request_id = format!("ext_{}", uuid::Uuid::new_v4().simple());
            let rx = ctx.external_requests.register(&request_id);
            ctx.external_requests.emit(AgentEventKind::ExternalAccessRequest {
                request_id: request_id.clone(),
                path: path.clone(),
                reason: reason.clone(),
                kind,
            });
            receivers.push((request_id, path, rx));
        }

        if receivers.is_empty() {
            let detail = if invalid_paths.is_empty() {
                "No valid external paths requested.".to_string()
            } else {
                format!("No valid external paths requested. Rejected: {}.", invalid_paths.join(", "))
            };
            return Ok(ToolResult {
                success: false,
                output: detail,
                is_error: true,
            });
        }

        // Await all approvals under ONE shared 5-minute budget (not 5 min per path).
        // A cancellation (`agent_cancel_run`) must interrupt the wait immediately —
        // the previous code only polled the oneshot receiver, leaving the run
        // blocked up to 5 minutes on an unanswered popup.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
        let mut allowed_paths: Vec<String> = Vec::new();
        let mut denied_paths: Vec<String> = invalid_paths;
        let all_request_ids: Vec<String> = receivers.iter().map(|(id, _, _)| id.clone()).collect();
        for (request_id, path, rx) in receivers {
            let allowed = tokio::select! {
                r = rx => match r {
                    Ok(true) => true,
                    Ok(false) => false,
                    Err(_) => false,   // sender dropped (run cancelled)
                },
                _ = ctx.cancel.notified() => false,          // run cancelled while awaiting
                _ = tokio::time::sleep_until(deadline) => false, // shared budget exhausted
            };
            // Regardless of the outcome, drop the stale registry entry so a
            // later approve/deny can never resolve a dead request.
            ctx.external_requests.remove(&request_id);
            // If the run was cancelled while waiting for approval, stop waiting
            // for the remaining paths (cancel_all clears the pending map).
            if ctx.cancel.is_cancelled() {
                denied_paths.push(format!("{} (run cancelled while awaiting approval)", path));
                break;
            }
            ctx.external_requests.emit(AgentEventKind::ExternalAccessResolved {
                request_id,
                allowed,
            });
            if allowed {
                // Add to the kernel allowlist so subsequent reads in this run succeed.
                // Broad roots (/, ~, system trees) are refused by the kernel even
                // after a user click — treat the refusal as a denial.
                let abs = PathBuf::from(&path);
                match crate::agent::rlm_kernel::get_rlm_manager()
                    .add_allowed_root(&abs)
                    .await
                {
                    Ok(()) => allowed_paths.push(path),
                    Err(e) => {
                        tracing::warn!("External access approval refused: {}", e);
                        denied_paths.push(format!("{} (refused: overly broad path)", path));
                    }
                }
            } else {
                denied_paths.push(path);
            }
        }

        // Drain every registered request id (resolved ones are already removed
        // — this is a no-op for them; cancelled/skipped ones are purged so no
        // stale entry can linger or be resolved later).
        for request_id in &all_request_ids {
            ctx.external_requests.remove(request_id);
        }

        let summary = format!(
            "External access resolved. Allowed (re-run your read now): [{}]. Denied/timeout: [{}].",
            allowed_paths.join(", "),
            denied_paths.join(", ")
        );
        Ok(ToolResult {
            success: !allowed_paths.is_empty(),
            output: summary,
            is_error: allowed_paths.is_empty() && !denied_paths.is_empty(),
        })
    }
}

// 12. Code Outline Tool (Tree-sitter symbol outline for compact exploration)
pub struct CodeOutlineTool;

#[async_trait]
impl Tool for CodeOutlineTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "code_outline".to_string(),
            description: "Generates a compact structural symbol outline (functions, structs, classes, interfaces) using Tree-sitter without returning body code lines.".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Optional file or directory path relative to project root (default: whole project)" },
                    "max_symbols": { "type": "integer", "description": "Optional maximum symbols to return (default: 200)" }
                }
            }),
            requires_approval: false,
        }
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let rel_path_str = params.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let max_symbols = params.get("max_symbols").and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(200);

        // The path must stay inside the project (same scope rule as every other
        // read tool). `PathBuf::join` discards the root for absolute inputs,
        // so validate explicitly instead of trusting the caller. The canonical
        // result is used ONLY as the scope check; the original path is walked
        // (ignore's walker does not follow symlinks by default) so the output
        // keeps the same project-relative display as before.
        let candidate = if rel_path_str == "." || rel_path_str.is_empty() {
            ctx.project_root.clone()
        } else {
            let pb = PathBuf::from(rel_path_str);
            if pb.is_absolute() {
                return Ok(ToolResult {
                    success: false,
                    output: format!(
                        "Invalid path '{}': code_outline paths must be RELATIVE to the project root.",
                        rel_path_str
                    ),
                    is_error: true,
                });
            }
            ctx.project_root.join(pb)
        };
        if crate::security::PathGuard::validate_path_in_scope(&candidate, &ctx.project_root).is_err()
        {
            return Ok(ToolResult {
                success: false,
                output: format!(
                    "Invalid path '{}': must stay inside the project root.",
                    rel_path_str
                ),
                is_error: true,
            });
        }
        let target_path = candidate;

        let mut symbols = Vec::new();

        if target_path.is_file() {
            if let Ok(content) = std::fs::read_to_string(&target_path) {
                if let Ok(syms) = crate::indexer::ast::AstParser::parse_symbols(&target_path, &content, &ctx.project_root) {
                    symbols.extend(syms);
                }
            }
        } else if target_path.is_dir() {
            // Skip hidden files/dirs (`.env`, `.git`, `.ssh` leftovers inside
            // the project) — symbol outlines never need dotfiles, and this
            // mirrors the grep tool's ripgrep default of skipping them.
            let walker = ignore::WalkBuilder::new(&target_path)
                .hidden(true)
                .git_ignore(true)
                .build();

            for entry in walker.flatten() {
                if symbols.len() >= max_symbols {
                    break;
                }
                if entry.file_type().map_or(false, |ft| ft.is_file()) {
                    let file_p = entry.path();
                    let ext = file_p.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if matches!(ext, "rs" | "py" | "ts" | "tsx" | "js" | "jsx") {
                        if let Ok(content) = std::fs::read_to_string(file_p) {
                            if let Ok(syms) = crate::indexer::ast::AstParser::parse_symbols(file_p, &content, &ctx.project_root) {
                                symbols.extend(syms);
                            }
                        }
                    }
                }
            }
        } else {
            return Ok(ToolResult {
                success: false,
                output: format!("Path '{}' does not exist.", rel_path_str),
                is_error: true,
            });
        }

        if symbols.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No code symbols found in specified target path.".to_string(),
                is_error: false,
            });
        }

        symbols.truncate(max_symbols);
        let mut output = String::new();
        for s in &symbols {
            let rel = s.file_path.strip_prefix(&ctx.project_root).unwrap_or(&s.file_path);
            let kind_label = match s.kind {
                crate::indexer::ast::SymbolKind::Function => "func",
                crate::indexer::ast::SymbolKind::Method => "method",
                crate::indexer::ast::SymbolKind::Class => "class",
                crate::indexer::ast::SymbolKind::Struct => "struct",
                crate::indexer::ast::SymbolKind::Interface => "interface",
                crate::indexer::ast::SymbolKind::Enum => "enum",
                crate::indexer::ast::SymbolKind::Variable => "var",
                crate::indexer::ast::SymbolKind::Module => "mod",
            };
            output.push_str(&format!("{}:{} [{}] {}\n", rel.to_string_lossy(), s.start_line, kind_label, s.name));
        }

        Ok(ToolResult {
            success: true,
            output,
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filtered_definitions_respect_allowed_list() {
        let registry = ToolRegistry::new();
        let allowed = vec!["grep_search".to_string(), "submit_plan".to_string()];
        let defs = registry.get_definitions_filtered(&allowed);
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "grep_search");
        assert_eq!(defs[1].name, "submit_plan");
    }

    #[test]
    fn test_all_role_tools_registered() {
        let registry = ToolRegistry::new();
        for name in [
            "batch_file_read",
            "multi_replace_file",
            "write_file",
            "list_dir",
            "grep_search",
            "run_command",
            "submit_plan",
            "submit_verdict",
            "submit_audit",
            "submit_brief",
            "request_external_access",
            "rlm_python",
            "code_outline",
        ] {
            let defs = registry.get_definitions_filtered(&[name.to_string()]);
            assert_eq!(defs.len(), 1, "tool {} must be registered", name);
        }
    }

    #[test]
    fn test_destructive_command_guard() {
        assert!(is_destructive_command("rm -rf /"));
        assert!(is_destructive_command("sudo rm -fr /* --no-preserve-root"));
        assert!(is_destructive_command("mkfs.ext4 /dev/sda1"));
        assert!(!is_destructive_command("npm test"));
        assert!(!is_destructive_command("cargo build"));

        // Obfuscated broad targets the old substring guard missed.
        assert!(is_destructive_command("rm -rf ~"));
        assert!(is_destructive_command("rm -rf ~/*"));
        assert!(is_destructive_command("rm -fr ."));
        assert!(is_destructive_command("rm -rf .."));
        assert!(is_destructive_command("rm -rf $HOME"));
        assert!(is_destructive_command("rm -rf \"$HOME\""));
        assert!(is_destructive_command("rm -rf /home"));
        assert!(is_destructive_command("rm -rf /Users/macmini"));
        assert!(is_destructive_command("rm --recursive -f /etc"));
        assert!(is_destructive_command("rm -r --no-preserve-root /"));

        // Recursive flags placed AFTER the target must still be caught.
        assert!(is_destructive_command("rm -f /etc -r"));
        assert!(is_destructive_command("rm -f ~ -R"));
        assert!(is_destructive_command("rm -f / --no-preserve-root -r"));
        assert!(is_destructive_command("rm -v /home -Rf"));

        // Fork bombs in any naming/spacing variant.
        assert!(is_destructive_command(":(){ :|:& };:"));
        assert!(is_destructive_command("f(){ f|f& }; f"));
        assert!(is_destructive_command("bomb() { bomb | bomb & } ; bomb"));

        // Raw device writers.
        assert!(is_destructive_command("dd if=img.iso of=/dev/sda"));
        assert!(is_destructive_command("shred /dev/sda"));

        // Legitimate scoped commands must still pass.
        assert!(!is_destructive_command("rm -rf target/debug"));
        assert!(!is_destructive_command("rm -rf ./node_modules"));
        assert!(!is_destructive_command("rm -rf ~/Downloads/old-build")); // narrow subdir
        assert!(!is_destructive_command("rm file.txt")); // no recursion
        assert!(!is_destructive_command("grep -rn foo() src/")); // parens, not a bomb
    }

    #[tokio::test]
    async fn test_run_command_cwd_must_stay_in_project() {
        let temp_dir = std::env::temp_dir().join("kuda_cwd_guard_test");
        let project_root = temp_dir.join("project");
        let app_data = temp_dir.join("app_data");
        let _ = std::fs::create_dir_all(&project_root);
        let _ = std::fs::create_dir_all(&app_data);

        let ctx = ToolContext {
            project_root: project_root.clone(),
            app_data_dir: app_data.clone(),
            external_requests: Arc::new(ExternalRequestRegistry::new()),
            plan_decisions: Arc::new(PlanDecisionRegistry::new()),
            direction_decisions: Arc::new(DirectionDecisionRegistry::new()),
            session_id: None,
            cancel: CancelFlag::new(),
        };
        let tool = RunCommandTool;

        // Absolute cwd: PathBuf::join would discard the project root entirely.
        let res = tool
            .execute(serde_json::json!({"command": "pwd", "cwd": "/etc"}), &ctx)
            .await
            .unwrap();
        assert!(res.is_error, "absolute cwd must be rejected: {}", res.output);

        // Parent traversal climbs out of the project.
        let res = tool
            .execute(serde_json::json!({"command": "pwd", "cwd": "../.."}), &ctx)
            .await
            .unwrap();
        assert!(res.is_error, "escaping cwd must be rejected: {}", res.output);

        // A valid in-project cwd still works.
        let res = tool
            .execute(serde_json::json!({"command": "pwd"}), &ctx)
            .await
            .unwrap();
        assert!(!res.is_error, "default cwd must work: {}", res.output);
        assert!(res.output.contains("project"), "pwd should run in project: {}", res.output);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_code_outline_path_must_stay_in_project() {
        let temp_dir = std::env::temp_dir().join("kuda_outline_scope_test");
        let project_root = temp_dir.join("project");
        let app_data = temp_dir.join("app_data");
        let _ = std::fs::create_dir_all(&project_root);
        let _ = std::fs::create_dir_all(&app_data);
        let _ = std::fs::write(project_root.join("lib.rs"), "pub fn inside() {}\n");

        let ctx = ToolContext {
            project_root: project_root.clone(),
            app_data_dir: app_data.clone(),
            external_requests: Arc::new(ExternalRequestRegistry::new()),
            plan_decisions: Arc::new(PlanDecisionRegistry::new()),
            direction_decisions: Arc::new(DirectionDecisionRegistry::new()),
            session_id: None,
            cancel: CancelFlag::new(),
        };
        let tool = CodeOutlineTool;

        // Absolute path outside the project (e.g. /etc) must be rejected, not
        // walked via PathBuf::join discarding the project root.
        let res = tool
            .execute(serde_json::json!({"path": "/etc"}), &ctx)
            .await
            .unwrap();
        assert!(res.is_error, "absolute path must be rejected: {}", res.output);

        // Parent traversal out of the project.
        let res = tool
            .execute(serde_json::json!({"path": "../.."}), &ctx)
            .await
            .unwrap();
        assert!(res.is_error, "escaping path must be rejected: {}", res.output);

        // In-project path still works.
        let res = tool
            .execute(serde_json::json!({"path": "lib.rs"}), &ctx)
            .await
            .unwrap();
        assert!(!res.is_error, "in-project path must work: {}", res.output);
        assert!(res.output.contains("inside"), "{}", res.output);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_multi_replace_respects_line_range() {
        let temp_dir = std::env::temp_dir().join("kuda_multi_replace_test");
        let project_root = temp_dir.join("project");
        let app_data = temp_dir.join("app_data");
        let _ = std::fs::create_dir_all(&project_root);
        let _ = std::fs::create_dir_all(&app_data);
        let target = project_root.join("dup.txt");
        let _ = std::fs::write(&target, "a\nSAME\nb\nSAME\nc\n");

        let ctx = ToolContext {
            project_root: project_root.clone(),
            app_data_dir: app_data.clone(),
            external_requests: Arc::new(ExternalRequestRegistry::new()),
            plan_decisions: Arc::new(PlanDecisionRegistry::new()),
            direction_decisions: Arc::new(DirectionDecisionRegistry::new()),
            session_id: None,
            cancel: CancelFlag::new(),
        };
        let tool = MultiReplaceFileTool;
        let params = serde_json::json!({
            "target_file": "dup.txt",
            "replacement_chunks": [{
                "start_line": 2,
                "end_line": 2,
                "target_content": "SAME",
                "replacement_content": "A"
            }]
        });
        let res = tool.execute(params, &ctx).await.unwrap();
        assert!(res.success, "{}", res.output);
        let new_content = std::fs::read_to_string(&target).unwrap();
        assert!(
            new_content.contains("\nA\nb\nSAME\n"),
            "only line 2 must change, got: {:?}",
            new_content
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}

