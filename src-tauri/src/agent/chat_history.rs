use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;
use crate::error::{AppError, Result};
use crate::agent::llm_client::{Message, MessageRole};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatSessionMeta {
    pub session_id: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub message_count: usize,
}

/// A single tool call performed by a swarm phase, captured for display replay.
/// Display-only: this never enters the LLM context.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct PhaseToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub arguments_json: String,
    /// Truncated output (≤ `PHASE_TOOL_OUTPUT_CHARS`) — kept bounded for disk.
    pub output: String,
    /// "running" | "done" | "error"
    pub status: String,
}

/// One phase (role) of one swarm run, captured for history replay.
/// Display-only: this never enters the LLM context (the ledger messages do).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PhaseRecord {
    /// Groups every phase of a single run into one box in the UI.
    pub run_id: String,
    /// AgentRoleKey of the phase (e.g. "thinker", "rlm_model", "user_gate").
    pub role: String,
    pub label: String,
    pub model: String,
    pub summary: String,
    /// Accumulated streamed text of the phase (= the role's final text).
    pub text: String,
    /// Streamed reasoning/thinking of the phase (display-only).
    #[serde(default)]
    pub thinking: String,
    pub tool_calls: Vec<PhaseToolCall>,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatSessionData {
    pub meta: ChatSessionMeta,
    /// LLM context: user prompts + one ledger message per swarm turn. Append-only.
    pub messages: Vec<Message>,
    /// Display replay of every swarm phase per run. Never sent to the LLM.
    #[serde(default)]
    pub transcript: Vec<PhaseRecord>,
    pub checkpoint_ids: Vec<String>,
}

/// True when `id` is safe to use as a single file-name component: non-empty,
/// bounded, and restricted to `[A-Za-z0-9_-]`. Accepts UUID v4 session ids and
/// legacy ids like `legacy_sess`, but rejects any traversal payload
/// (`../hub_credentials`, absolute paths, separators, dots, null bytes).
pub fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub struct ChatHistoryManager {
    chat_dir: PathBuf,
}

/// Serializes every session read-modify-write so two concurrent appends to the
/// same session can never lose a message (the previous code loaded → mutated →
/// saved the whole file with no lock). Coarse (all sessions) but chat history
/// writes are rare and small; the lock is only held during load+save.
pub(crate) static SESSION_IO_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Atomic file write (temp + fsync + rename) so a crash mid-write can never
/// truncate a session file that holds the turn ledger / context.
fn write_file_atomic(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

impl ChatHistoryManager {
    pub fn new(app_data_dir: &Path) -> Result<Self> {
        let chat_dir = app_data_dir.join("chat_history");
        if !chat_dir.exists() {
            fs::create_dir_all(&chat_dir)?;
        }
        Ok(Self { chat_dir })
    }

    /// File path for a session id, rejecting anything that is not a plain
    /// single-component id. This is the path-traversal guard for every
    /// session file operation: a hostile `session_id` like
    /// `../hub_credentials` must never escape `chat_dir`.
    fn session_file_path(&self, session_id: &str) -> Result<PathBuf> {
        if !is_safe_id(session_id) {
            return Err(AppError::General(format!(
                "Invalid session id {:?}: must be a plain identifier without path separators",
                session_id
            )));
        }
        Ok(self.chat_dir.join(format!("{}.json", session_id)))
    }

    /// Creates a new persistent Chat Session
    pub fn create_session(&self, initial_title: Option<String>) -> Result<ChatSessionData> {
        let session_id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let title = initial_title.unwrap_or_else(|| "New Conversation".to_string());

        let meta = ChatSessionMeta {
            session_id: session_id.clone(),
            title,
            created_at: now,
            updated_at: now,
            message_count: 0,
        };

        let session_data = ChatSessionData {
            meta,
            messages: Vec::new(),
            transcript: Vec::new(),
            checkpoint_ids: Vec::new(),
        };

        self.save_session(&session_data)?;
        Ok(session_data)
    }

    /// Saves or updates a Chat Session JSON file on disk (atomic, serialized).
    pub fn save_session(&self, session: &ChatSessionData) -> Result<()> {
        let _guard = SESSION_IO_LOCK.lock().unwrap();
        self.save_session_unlocked(session)
    }

    /// Same as `save_session` but WITHOUT acquiring the session lock — callers
    /// that already hold `SESSION_IO_LOCK` (a full read-modify-write) must use
    /// this to avoid a deadlock on the non-reentrant mutex.
    pub fn save_session_unlocked(&self, session: &ChatSessionData) -> Result<()> {
        let file_path = self.session_file_path(&session.meta.session_id)?;
        let json = serde_json::to_string_pretty(session)?;
        write_file_atomic(&file_path, &json)?;
        Ok(())
    }

    /// Loads a specific Chat Session by ID
    pub fn load_session(&self, session_id: &str) -> Result<ChatSessionData> {
        let file_path = self.session_file_path(session_id)?;
        if !file_path.exists() {
            return Err(AppError::General(format!("Chat session {} not found", session_id)));
        }
        let content = fs::read_to_string(&file_path)?;
        let session: ChatSessionData = serde_json::from_str(&content)?;
        Ok(session)
    }

    /// Lists all chat history sessions ordered by updated_at descending
    pub fn list_sessions(&self) -> Result<Vec<ChatSessionMeta>> {
        let mut sessions = Vec::new();
        if self.chat_dir.exists() {
            for entry in fs::read_dir(&self.chat_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("json") {
                    if let Ok(content) = fs::read_to_string(&path) {
                        if let Ok(session) = serde_json::from_str::<ChatSessionData>(&content) {
                            sessions.push(session.meta);
                        }
                    }
                }
            }
        }
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(sessions)
    }

    /// Appends a message to an existing session and updates timestamp
    pub fn append_message(&self, session_id: &str, message: Message, checkpoint_id: Option<String>) -> Result<ChatSessionData> {
        let _guard = SESSION_IO_LOCK.lock().unwrap();
        let mut session = self.load_session(session_id)?;
        
        // Auto-generate title from first user message if default
        if session.meta.title == "New Conversation" && message.content.len() > 0 {
            let snippet = message.content.lines().next().unwrap_or(&message.content);
            let truncated = if snippet.chars().count() > 40 {
                format!("{}...", snippet.chars().take(40).collect::<String>())
            } else {
                snippet.to_string()
            };
            session.meta.title = truncated;
        }

        let mut message = message;
        if message.created_at.is_none() {
            message.created_at = Some(chrono::Local::now());
        }
        session.messages.push(message);
        if let Some(chk_id) = checkpoint_id {
            session.checkpoint_ids.push(chk_id);
        }

        session.meta.message_count = session.messages.len();
        session.meta.updated_at = Utc::now();

        self.save_session_unlocked(&session)?;
        Ok(session)
    }

    /// Appends a run's phase transcript to a session. Append-only: existing
    /// records are never rewritten (mirrors the ledger invariant).
    pub fn append_transcript(&self, session_id: &str, records: &[PhaseRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let _guard = SESSION_IO_LOCK.lock().unwrap();
        let mut session = self.load_session(session_id)?;
        session.transcript.extend(records.iter().cloned());
        session.meta.updated_at = Utc::now();
        self.save_session_unlocked(&session)?;
        Ok(())
    }

    /// Deletes a chat session file
    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        let file_path = self.session_file_path(session_id)?;
        if file_path.exists() {
            fs::remove_file(&file_path)?;
        }
        Ok(())
    }
}

/// Number of full turns kept verbatim before older turns are rolled into an
/// immutable `[EPOCH SUMMARY]` block. Fixed at 10 by explicit user decision:
/// fewer than 10 turns lose too much cross-turn context.
/// Minimum number of the MOST RECENT full turns always kept VERBATIM in the
/// context window — never summarized. Recent context stays intact for accuracy.
pub const RECENT_KEEP: usize = 5;
/// Number of old turns folded into ONE compact "previous chat history" block at
/// a time. Batches stay small (never "many turns into one") so summaries keep
/// enough detail to stay useful.
pub const COMPRESS_BATCH: usize = 5;
/// Approximate char budget of one compact history block.
pub const EPOCH_SUMMARY_CHARS: usize = 500;
/// When the estimated context window (history blocks + kept turns + prompt)
/// would exceed this many tokens, fold the oldest turns until it fits. The
/// window is therefore bounded in TOKENS — the thing that is actually billed —
/// regardless of how many turns a session accumulates.
pub const MAX_WINDOW_TOKENS: usize = 100_000;
/// Token budget for the most recent turns kept verbatim in the window (older
/// turns fold). At least `RECENT_KEEP` turns are always kept, even if that
/// exceeds this budget (accuracy floor).
pub const RECENT_TOKEN_BUDGET: usize = 30_000;
/// Token room reserved for the new prompt + the swarm's own additions, so the
/// kept turns stay within the model's context budget.
pub const PROMPT_SLACK_TOKENS: usize = 8_000;

/// Compacts old `[ledger]` turns into "previous chat history" blocks.
///
/// Window layout: `[previous chat history blocks][kept turns verbatim][new
/// prompt]`. Folding is driven by the TOKEN budget, not turn count: turns stay
/// verbatim until the estimated window would exceed `MAX_WINDOW_TOKENS`; then
/// the oldest turns fold (in batches of `COMPRESS_BATCH`) until the kept recent
/// turns fit `RECENT_TOKEN_BUDGET`. At least `RECENT_KEEP` newest turns are
/// always kept verbatim (accuracy floor).
///
/// Blocks are append-only (never rewritten) and the window's prefix (the blocks
/// + the fixed kept turns) is byte-identical between compressions, so a
/// provider's prefix/context cache stays warm for every turn in between.
///
/// Each block records the 0-based turn range it covers in `[COVERS] a-b`.
///
/// Priority when a summary would exceed its budget:
/// 1. `[PLAN STATUS]` + files edited + verdict (state of the world).
/// 2. Goal + one-line result per turn.
/// 3. Brief/plan detail — dropped first (re-research via rlm_cache is possible).
pub fn compact_epoch(session: &mut ChatSessionData) -> Result<()> {
    // Collect ledger messages (name == "ledger") in order, keeping the user
    // prompt that precedes each one for the goal line.
    let mut turns: Vec<(String, String)> = Vec::new(); // (user_prompt, ledger_text)
    let mut current_user = String::new();
    for m in &session.messages {
        match m.name.as_deref() {
            Some("epoch") => {}
            Some("ledger") => {
                turns.push((std::mem::take(&mut current_user), m.content.clone()));
            }
            _ => {
                if m.role == crate::agent::llm_client::MessageRole::User
                    && m.name.as_deref() != Some("epoch")
                {
                    current_user = m.content.clone();
                }
            }
        }
    }

    // How many turns are already covered by existing blocks.
    let existing_blocks: Vec<Message> = session
        .messages
        .iter()
        .filter(|m| m.name.as_deref() == Some("epoch"))
        .cloned()
        .collect();
    let summarized: usize = existing_blocks
        .iter()
        .map(|m| epoch_block_covers(m).unwrap_or(COMPRESS_BATCH))
        .sum();
    let non_summarized = turns.len().saturating_sub(summarized);
    if non_summarized <= RECENT_KEEP {
        return Ok(());
    }

    // Pure TOKEN-BASED folding. The window = [history blocks][kept turns][prompt].
    // Budget for the kept turns: at most RECENT_TOKEN_BUDGET tokens, but also
    // never so much that the whole window (blocks + turns + prompt slack) would
    // exceed MAX_WINDOW_TOKENS. Always keep at least RECENT_KEEP turns (floor).
    // Cheap conservative estimate (~3 chars/token for code) keeps this instant.
    let block_chars: usize = existing_blocks.iter().map(|b| b.content.chars().count()).sum();
    let window_budget_chars =
        (MAX_WINDOW_TOKENS - PROMPT_SLACK_TOKENS - block_chars / 3).saturating_mul(3);
    let recent_budget_chars = RECENT_TOKEN_BUDGET.saturating_mul(3);
    let budget_chars = window_budget_chars.min(recent_budget_chars);

    let mut keep_count = 0usize;
    let mut kept_chars = 0usize;
    for (prompt, ledger) in turns.iter().rev() {
        let c = prompt.chars().count() + ledger.chars().count();
        if keep_count >= RECENT_KEEP && kept_chars + c > budget_chars {
            break;
        }
        kept_chars += c;
        keep_count += 1;
    }
    let keep_count = keep_count.max(RECENT_KEEP);

    // Fold the oldest turns (in batches of COMPRESS_BATCH — never "many into
    // one") that are beyond the keep window.
    let fold_upto = non_summarized.saturating_sub(keep_count);
    let to_fold = fold_upto / COMPRESS_BATCH * COMPRESS_BATCH;
    if to_fold == 0 {
        return Ok(());
    }

    let mut new_blocks: Vec<Message> = Vec::new();
    let mut folded = 0usize;
    while folded < to_fold {
        let start = summarized + folded;
        // The guard's partial remainder may be smaller than a full batch.
        let end = (start + COMPRESS_BATCH).min(summarized + to_fold);
        if end > turns.len() {
            break;
        }
        let mut status = String::new();
        let mut user_lines: Vec<String> = Vec::new();
        for (i, (prompt, ledger)) in turns[start..end].iter().enumerate() {
            let one_line = prompt
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(80)
                .collect::<String>();
            user_lines.push(format!("  T{}: {}", i + 1, one_line));

            for line in ledger.lines() {
                let t = line.trim();
                if t.starts_with("[PLAN STATUS]") {
                    status.push_str(&format!("T{} {}\n", i + 1, t));
                } else if t.starts_with("[EXEC]") {
                    status.push_str(&format!("T{} {}\n", i + 1, t.chars().take(100).collect::<String>()));
                } else if t.starts_with("[VERDICT]") {
                    status.push_str(&format!("T{} {}\n", i + 1, t.chars().take(140).collect::<String>()));
                }
            }
        }
        let n = existing_blocks.len() + new_blocks.len() + 1;
        let mut body = String::new();
        body.push_str(&format!("[PREVIOUS CHAT HISTORY — SUMMARY {}]\n", n));
        body.push_str(&format!("[COVERS] {}-{}\n", start, end));
        if !status.is_empty() {
            body.push_str(&format!("[STATE]\n{}\n", status.trim_end()));
        }
        let goal_joined = user_lines.join("\n");
        body.push_str(&format!("[GOAL]\n{}", goal_joined.trim_end()));

        if body.chars().count() > EPOCH_SUMMARY_CHARS {
            let keep_status = status;
            let keep_goal = user_lines;
            // Budget: keep STATE (priority 1-2) then as much GOAL as fits.
            let mut bounded = format!(
                "[PREVIOUS CHAT HISTORY — SUMMARY {}]\n[COVERS] {}-{}\n[STATE]\n{}",
                n,
                start,
                end,
                keep_status.trim_end()
            );
            let remaining = EPOCH_SUMMARY_CHARS.saturating_sub(bounded.chars().count());
            if remaining > 0 {
                let joined = keep_goal.join("\n");
                bounded.push_str(&format!("\n[GOAL]\n{}", truncate_for_epoch(&joined, remaining)));
            }
            body = bounded;
        }

        new_blocks.push(Message {
            role: crate::agent::llm_client::MessageRole::User,
            content: body,
            name: Some("epoch".to_string()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            created_at: Some(chrono::Local::now()),
        });
        folded += COMPRESS_BATCH;
    }

    if new_blocks.is_empty() {
        return Ok(());
    }
    // Insert new blocks AFTER any existing blocks so the sequence stays
    // chronological: [summary 1][summary 2]...[kept turns] — the newest summary
    // is appended to the prefix, so the older prefix stays cacheable.
    let epoch_prefix_end = session
        .messages
        .iter()
        .take_while(|m| m.name.as_deref() == Some("epoch"))
        .count();
    let mut out: Vec<Message> = Vec::with_capacity(session.messages.len() + new_blocks.len());
    out.extend(session.messages.iter().take(epoch_prefix_end).cloned());
    out.extend(new_blocks);
    out.extend(session.messages.iter().skip(epoch_prefix_end).cloned());
    session.messages = out;
    Ok(())
}

/// Number of turns a compact history block covers, read from its `[COVERS] a-b`
/// line. Returns `None` for legacy blocks that predate the marker.
fn epoch_block_covers(m: &Message) -> Option<usize> {
    let line = m.content.lines().find(|l| l.starts_with("[COVERS] "))?;
    let range = line.trim_start_matches("[COVERS] ");
    let (a, b) = range.split_once('-')?;
    let a: usize = a.trim().parse().ok()?;
    let b: usize = b.trim().parse().ok()?;
    Some(b.saturating_sub(a))
}

fn truncate_for_epoch(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// Builds the LLM context for the next swarm turn from a session's messages.
///
/// Window layout: `[previous chat history blocks][kept turn pairs verbatim]
/// [new prompt]`. The compact blocks are emitted at the front (stable prefix),
/// the turns they cover are skipped, and the newest turns (the keep window) are
/// sent verbatim. The session file itself stays append-only — only the window
/// sent to the LLM is filtered.
pub fn build_ledger_context(messages: &[Message], new_user: &Message) -> Vec<Message> {
    // Turns covered by compact history blocks (read from each block's [COVERS]).
    let summarized_turns: usize = messages
        .iter()
        .filter(|m| m.name.as_deref() == Some("epoch"))
        .map(|m| epoch_block_covers(m).unwrap_or(COMPRESS_BATCH))
        .sum();

    let mut out: Vec<Message> = Vec::with_capacity(messages.len() + 1);
    let mut turn_index = 0usize;

    for m in messages {
        match m.name.as_deref() {
            Some("epoch") => {
                // The compact history blocks — stable prefix, sent verbatim.
                out.push(m.clone());
            }
            Some("ledger") => {
                if turn_index < summarized_turns {
                    // This ledger (and its preceding user prompt) is already
                    // represented inside a compact block — skip both.
                    turn_index += 1;
                } else {
                    out.push(m.clone());
                    turn_index += 1;
                }
            }
            Some("error") => {
                // Diagnostic "Run failed: ..." messages from failed runs carry no
                // facts the model needs — they would otherwise be appended to the
                // LLM window forever (even after the turn is summarized into an
                // epoch block), inflating context and nudging the model to repeat
                // stale failure text.
                continue;
            }
            _ => {
                if m.role == MessageRole::User {
                    if turn_index < summarized_turns {
                        // Summarized turn's user prompt — skip.
                    } else {
                        out.push(m.clone());
                    }
                } else {
                    out.push(m.clone());
                }
            }
        }
    }

    out.push(new_user.clone());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_id_traversal_is_rejected() {
        let temp_dir = std::env::temp_dir().join("kuda_chat_traversal_test");
        let _ = fs::remove_dir_all(&temp_dir);
        let mgr = ChatHistoryManager::new(&temp_dir).unwrap();

        // Hostile ids must never escape chat_history/ (e.g. overwriting
        // hub_credentials.json or deleting arbitrary .json files).
        for evil in [
            "../hub_credentials",
            "../../etc/passwd",
            "/abs/path",
            "a/b",
            "a\\b",
            "..",
            ".",
            "",
        ] {
            assert!(
                mgr.load_session(evil).is_err(),
                "load_session({:?}) must be rejected",
                evil
            );
            assert!(
                mgr.delete_session(evil).is_err(),
                "delete_session({:?}) must be rejected",
                evil
            );
        }

        // No file was created outside chat_history/, and hub_credentials.json
        // in app-data was never touched.
        assert!(!temp_dir.join("hub_credentials.json").exists());

        // A save with a hostile session id must also be rejected.
        let session = ChatSessionData {
            meta: ChatSessionMeta {
                session_id: "../evil".to_string(),
                title: "t".to_string(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                message_count: 0,
            },
            messages: Vec::new(),
            transcript: Vec::new(),
            checkpoint_ids: Vec::new(),
        };
        assert!(mgr.save_session(&session).is_err());
        assert!(!temp_dir.join("evil.json").exists());

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_chat_history_lifecycle() {
        let temp_dir = std::env::temp_dir().join("kuda_chat_test");
        let _ = fs::create_dir_all(&temp_dir);

        let mgr = ChatHistoryManager::new(&temp_dir).unwrap();

        // 1. Create Session
        let session = mgr.create_session(None).unwrap();
        assert_eq!(session.meta.title, "New Conversation");

        // 2. Append User Message
        let msg = Message::user("Tolong buatkan fungsi login");
        let updated = mgr.append_message(&session.meta.session_id, msg, Some("chk_123".to_string())).unwrap();
        assert_eq!(updated.meta.title, "Tolong buatkan fungsi login");
        assert_eq!(updated.messages.len(), 1);
        assert_eq!(updated.checkpoint_ids.len(), 1);

        // 3. List Sessions
        let list = mgr.list_sessions().unwrap();
        assert_eq!(list.len(), 1);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    fn sample_record(run_id: &str, role: &str) -> PhaseRecord {
        PhaseRecord {
            run_id: run_id.to_string(),
            role: role.to_string(),
            label: "planning".to_string(),
            model: "m".to_string(),
            summary: "ok".to_string(),
            text: "thinking".to_string(),
            thinking: String::new(),
            tool_calls: vec![PhaseToolCall {
                call_id: "c1".into(),
                tool_name: "grep_search".into(),
                arguments_json: "{}".into(),
                output: "out".into(),
                status: "done".into(),
            }],
            created_at: Utc::now(),
        }
    }

    #[test]
    fn test_append_transcript_is_append_only_and_legacy_loads_empty() {
        let temp_dir = std::env::temp_dir().join("kuda_chat_transcript_test");
        let _ = fs::remove_dir_all(&temp_dir);
        let mgr = ChatHistoryManager::new(&temp_dir).unwrap();
        let session = mgr.create_session(None).unwrap();
        let recs = vec![sample_record("run1", "thinker")];

        mgr.append_transcript(&session.meta.session_id, &recs).unwrap();
        let loaded = mgr.load_session(&session.meta.session_id).unwrap();
        assert_eq!(loaded.transcript.len(), 1);

        // A second append must EXTEND, never rewrite the earlier record.
        mgr.append_transcript(&session.meta.session_id, &recs).unwrap();
        let loaded2 = mgr.load_session(&session.meta.session_id).unwrap();
        assert_eq!(loaded2.transcript.len(), 2);
        assert_eq!(loaded2.transcript[0].role, "thinker");

        // Legacy session file without the `transcript` field loads with an
        // empty transcript (fail-open, backward compatible).
        let file = temp_dir.join("chat_history").join("legacy_sess.json");
        fs::create_dir_all(temp_dir.join("chat_history")).unwrap();
        fs::write(
            &file,
            r#"{"meta":{"session_id":"legacy_sess","title":"t","created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:00:00Z","message_count":0},"messages":[],"checkpoint_ids":[]}"#,
        )
        .unwrap();
        let legacy = mgr.load_session("legacy_sess").unwrap();
        assert!(legacy.transcript.is_empty());

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_compact_epoch_folds_heavy_turns_once_and_keeps_overrides() {
        let temp_dir = std::env::temp_dir().join("kuda_epoch_compact_test");
        let _ = fs::remove_dir_all(&temp_dir);
        let mgr = ChatHistoryManager::new(&temp_dir).unwrap();
        let session = mgr.create_session(None).unwrap();
        let sid = session.meta.session_id.clone();

        // Token-heavy turns (each ledger ~60k chars) push the window over the
        // budget, so the oldest batch folds (COMPRESS_BATCH turns per block).
        let big = "x".repeat(60_000);
        for i in 0..(RECENT_KEEP + COMPRESS_BATCH) {
            let prompt = format!("user prompt number {}", i);
            mgr.append_message(&sid, Message::user(prompt), None).unwrap();
            let ledger_msg = Message {
                role: crate::agent::llm_client::MessageRole::Assistant,
                content: format!(
                    "[TURN LEDGER]\n{}\n[PLAN STATUS] approved (no changes)\n\
                     [EXEC] Task #1 (code) — changed src/a.rs\n[VERDICT] PASSED\n\
                     [FINAL ANSWER] done turn {}",
                    big, i
                ),
                name: Some("ledger".to_string()),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                created_at: None,
            };
            mgr.append_message(&sid, ledger_msg, None).unwrap();
        }

        let mut s = mgr.load_session(&sid).unwrap();
        compact_epoch(&mut s).unwrap();
        let epochs: Vec<_> = s.messages.iter().filter(|m| m.name.as_deref() == Some("epoch")).collect();
        assert_eq!(epochs.len(), 1);
        assert!(epochs[0].content.contains("[PREVIOUS CHAT HISTORY"));
        assert!(epochs[0].content.contains("[COVERS] 0-5"));
        assert!(epochs[0].content.contains("[PLAN STATUS] approved"));
        assert!(epochs[0].content.contains("[VERDICT] PASSED"));

        // Re-running must NOT create a duplicate epoch block.
        compact_epoch(&mut s).unwrap();
        let epochs2: Vec<_> = s.messages.iter().filter(|m| m.name.as_deref() == Some("epoch")).collect();
        assert_eq!(epochs2.len(), 1);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_build_ledger_context_windows_summarized_turns() {
        let temp_dir = std::env::temp_dir().join("kuda_ledger_ctx_test");
        let _ = fs::remove_dir_all(&temp_dir);
        let mgr = ChatHistoryManager::new(&temp_dir).unwrap();
        let session = mgr.create_session(None).unwrap();
        let sid = session.meta.session_id.clone();

        let big = "x".repeat(60_000);
        for i in 0..(RECENT_KEEP + COMPRESS_BATCH) {
            mgr.append_message(&sid, Message::user(format!("user {}", i)), None).unwrap();
            mgr.append_message(
                &sid,
                Message {
                    role: crate::agent::llm_client::MessageRole::Assistant,
                    content: format!("[TURN LEDGER]\n{}\n[FINAL ANSWER] done {}", big, i),
                    name: Some("ledger".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    created_at: None,
                },
                None,
            )
            .unwrap();
        }

        let mut s = mgr.load_session(&sid).unwrap();
        compact_epoch(&mut s).unwrap();
        mgr.save_session(&s).unwrap();

        // After the fold: window = [history block][kept turns verbatim][prompt].
        // Blocks are front messages; the prompt stays plain.
        let ctx = build_ledger_context(&s.messages, &Message::user("new prompt"));
        let epochs = ctx.iter().filter(|m| m.name.as_deref() == Some("epoch")).count();
        let ledgers = ctx.iter().filter(|m| m.name.as_deref() == Some("ledger")).count();
        assert_eq!(epochs, 1);
        assert_eq!(ledgers, RECENT_KEEP);
        assert_eq!(ctx.first().unwrap().name.as_deref(), Some("epoch"));
        assert_eq!(ctx.last().unwrap().content, "new prompt");

        // More turns stay verbatim until the next fold.
        for i in 0..2 {
            mgr.append_message(&sid, Message::user(format!("user more {}", i)), None).unwrap();
            mgr.append_message(
                &sid,
                Message {
                    role: crate::agent::llm_client::MessageRole::Assistant,
                    content: format!("[TURN LEDGER]\n{}\n[FINAL ANSWER] more {}", big, i),
                    name: Some("ledger".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    created_at: None,
                },
                None,
            )
            .unwrap();
        }
        let s2 = mgr.load_session(&sid).unwrap();
        let ctx2 = build_ledger_context(&s2.messages, &Message::user("again"));
        let ledgers2 = ctx2.iter().filter(|m| m.name.as_deref() == Some("ledger")).count();
        assert_eq!(ledgers2, RECENT_KEEP + 2);
        assert_eq!(ctx2.last().unwrap().content, "again");
        // History block stays first.
        assert_eq!(ctx2.first().unwrap().name.as_deref(), Some("epoch"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_compact_epoch_small_turns_stay_verbatim() {
        // Pure token-based: many SMALL turns stay verbatim (no fold) because the
        // window is far below the token budget — no arbitrary turn-count folding.
        let temp_dir = std::env::temp_dir().join("kuda_small_turns_test");
        let _ = fs::remove_dir_all(&temp_dir);
        let mgr = ChatHistoryManager::new(&temp_dir).unwrap();
        let session = mgr.create_session(None).unwrap();
        let sid = session.meta.session_id.clone();

        for i in 0..30 {
            mgr.append_message(&sid, Message::user(format!("user {}", i)), None).unwrap();
            mgr.append_message(
                &sid,
                Message {
                    role: crate::agent::llm_client::MessageRole::Assistant,
                    content: format!("[TURN LEDGER]\n[FINAL ANSWER] done {}", i),
                    name: Some("ledger".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    created_at: None,
                },
                None,
            )
            .unwrap();
        }

        let mut s = mgr.load_session(&sid).unwrap();
        compact_epoch(&mut s).unwrap();
        let epochs: Vec<_> = s.messages.iter().filter(|m| m.name.as_deref() == Some("epoch")).collect();
        assert_eq!(epochs.len(), 0, "small turns never fold under the token budget");
        let ctx = build_ledger_context(&s.messages, &Message::user("next"));
        let ledgers = ctx.iter().filter(|m| m.name.as_deref() == Some("ledger")).count();
        assert_eq!(ledgers, 30);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_compact_epoch_keeps_floor_when_over_budget() {
        // Even when every turn is huge (window over budget), folding never drops
        // below the RECENT_KEEP accuracy floor.
        let temp_dir = std::env::temp_dir().join("kuda_token_fold_test");
        let _ = fs::remove_dir_all(&temp_dir);
        let mgr = ChatHistoryManager::new(&temp_dir).unwrap();
        let session = mgr.create_session(None).unwrap();
        let sid = session.meta.session_id.clone();

        let big = "x".repeat(60_000);
        for i in 0..12 {
            mgr.append_message(&sid, Message::user(format!("user {}", i)), None).unwrap();
            mgr.append_message(
                &sid,
                Message {
                    role: crate::agent::llm_client::MessageRole::Assistant,
                    content: format!("[TURN LEDGER]\n{} done {}", big, i),
                    name: Some("ledger".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    created_at: None,
                },
                None,
            )
            .unwrap();
        }

        let mut s = mgr.load_session(&sid).unwrap();
        compact_epoch(&mut s).unwrap();
        let epochs: Vec<_> = s.messages.iter().filter(|m| m.name.as_deref() == Some("epoch")).collect();
        assert!(!epochs.is_empty(), "over-budget turns must fold");
        let folded: usize = epochs
            .iter()
            .map(|m| epoch_block_covers(m).unwrap_or(0))
            .sum();
        let kept = 12 - folded;
        assert!(kept >= RECENT_KEEP, "kept window must respect the floor");

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
