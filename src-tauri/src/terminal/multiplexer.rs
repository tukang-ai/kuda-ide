use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use tauri::ipc::Channel;
use crate::error::{AppError, Result};
use crate::terminal::pty_manager::{PtySession, TerminalOutputPayload};

#[derive(Default)]
pub struct TerminalMultiplexer {
    sessions: Mutex<HashMap<String, PtySession>>,
}

impl TerminalMultiplexer {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn spawn_session<P: AsRef<Path>>(
        &self,
        cwd: P,
        cols: u16,
        rows: u16,
        on_output: Channel<TerminalOutputPayload>,
    ) -> Result<String> {
        let session = PtySession::spawn(cwd, cols, rows, on_output)?;
        let session_id = session.id.clone();

        let mut sessions = self.sessions.lock().unwrap();
        sessions.insert(session_id.clone(), session);

        tracing::info!("Spawned new terminal session: {}", session_id);
        Ok(session_id)
    }

    /// Clones the session handle out of the map WITHOUT holding the lock
    /// during blocking PTY I/O. A stuck shell (Ctrl-S/XOFF flow control, a
    /// stopped process group, a full PTY buffer) blocks `write_all` /
    /// `kill().wait()` indefinitely; holding the sessions mutex through that
    /// deadlocked EVERY terminal operation (spawn, kill, list, other tabs).
    fn get_cloned(&self, session_id: &str) -> Result<PtySession> {
        self.sessions
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .ok_or_else(|| AppError::General(format!("Session {} not found", session_id)))
    }

    pub fn write_to_session(&self, session_id: &str, data: &str) -> Result<()> {
        let session = self.get_cloned(session_id)?;
        // Map lock already released; only this session's writer is contended.
        session.write_bytes(data.as_bytes())
    }

    pub fn resize_session(&self, session_id: &str, cols: u16, rows: u16) -> Result<()> {
        let session = self.get_cloned(session_id)?;
        session.resize(cols, rows)
    }

    /// Terminates the shell process for a session and removes it from the map.
    /// Unlike the old implementation (which only dropped the map entry and left
    /// the shell running), this reliably kills and reaps the child process.
    pub fn kill_session(&self, session_id: &str) -> Result<()> {
        // Remove under the lock, then kill+reap AFTER releasing it: `wait()`
        // blocks until the child exits.
        let removed = self.sessions.lock().unwrap().remove(session_id);
        if let Some(session) = removed {
            let _ = session.kill();
            tracing::info!("Killed terminal session: {}", session_id);
            Ok(())
        } else {
            Err(AppError::General(format!("Session {} not found", session_id)))
        }
    }

    /// Removes sessions whose shell has exited on its own (user typed `exit`
    /// or the PTY master closed), so they cannot accumulate as dead tabs.
    /// Returns the ids that were reaped.
    pub fn reap_dead_sessions(&self) -> Vec<String> {
        let mut sessions = self.sessions.lock().unwrap();
        let dead: Vec<String> = sessions
            .iter()
            .filter(|(_, s)| !s.is_alive())
            .map(|(id, _)| id.clone())
            .collect();
        for id in &dead {
            sessions.remove(id);
        }
        dead
    }

    /// Snapshot of the currently registered session ids.
    pub fn session_ids(&self) -> Result<Vec<String>> {
        let sessions = self.sessions.lock().unwrap();
        Ok(sessions.keys().cloned().collect())
    }

    /// Kills every live session (used when the terminal panel is closed so no
    /// shell process outlives the UI).
    pub fn kill_all(&self) {
        // Drain under the lock, then kill+reap AFTER releasing it.
        let drained: Vec<(String, PtySession)> =
            self.sessions.lock().unwrap().drain().collect();
        for (id, session) in drained {
            let _ = session.kill();
            tracing::info!("Killed terminal session on panel close: {}", id);
        }
    }
}
