use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use tauri::ipc::Channel;
use uuid::Uuid;
use crate::error::{AppError, Result};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TerminalOutputPayload {
    pub session_id: String,
    pub data: String, // UTF-8 or Base64 String chunk
    pub is_base64: bool,
}

/// A single PTY-backed terminal session. Cheaply clonable (all state lives
/// behind `Arc`s) so the multiplexer can clone a handle out of the sessions
/// map and release the map lock BEFORE doing any blocking PTY I/O.
#[derive(Clone)]
pub struct PtySession {
    pub id: String,
    pub master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// The spawned shell process. Kept alive so `kill` can terminate it
    /// reliably (removing the session from the map alone leaves a zombie
    /// shell that keeps running).
    child: Arc<Mutex<Option<Box<dyn portable_pty::Child + Send + Sync>>>>,
}

impl PtySession {
    pub fn spawn<P: AsRef<Path>>(
        cwd: P,
        cols: u16,
        rows: u16,
        on_output: Channel<TerminalOutputPayload>,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AppError::General(format!("Failed to open PTY: {}", e)))?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.arg("-l");
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.cwd(cwd.as_ref());

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| AppError::General(format!("Failed to spawn shell '{}': {}", shell, e)))?;

        let session_id = Uuid::new_v4().to_string();
        let master = Arc::new(Mutex::new(pair.master));
        let reader = master.lock().unwrap().try_clone_reader().map_err(|e| {
            AppError::General(format!("Failed to clone PTY reader: {}", e))
        })?;

        let writer = Arc::new(Mutex::new(master.lock().unwrap().take_writer().map_err(|e| {
            AppError::General(format!("Failed to take PTY writer: {}", e))
        })?));

        // Spawn PTY reader thread
        let session_id_clone = session_id.clone();
        thread::spawn(move || {
            let mut reader = reader;
            let mut buffer = [0u8; 4096];

            loop {
                match reader.read(&mut buffer) {
                    Ok(n) if n > 0 => {
                        let data_str = String::from_utf8_lossy(&buffer[..n]).to_string();
                        let payload = TerminalOutputPayload {
                            session_id: session_id_clone.clone(),
                            data: data_str,
                            is_base64: false,
                        };

                        if on_output.send(payload).is_err() {
                            tracing::warn!("Terminal channel subscriber disconnected for {}", session_id_clone);
                            break;
                        }
                    }
                    Ok(_) => break, // EOF (shell exited)
                    Err(e) => {
                        tracing::debug!("PTY reader exited for {}: {}", session_id_clone, e);
                        break;
                    }
                }
            }
        });

        Ok(Self {
            id: session_id,
            master,
            writer,
            child: Arc::new(Mutex::new(Some(child))),
        })
    }

    pub fn write_bytes(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer
            .write_all(bytes)
            .map_err(|e| AppError::General(format!("Failed to write to PTY: {}", e)))?;
        writer
            .flush()
            .map_err(|e| AppError::General(format!("Failed to flush PTY: {}", e)))?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let master = self.master.lock().unwrap();
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AppError::General(format!("Failed to resize PTY: {}", e)))?;
        Ok(())
    }

    /// Kills the shell process (and its process group) and reaps it. This is
    /// the reliable termination path — dropping the session map entry alone
    /// used to leave the shell running as a zombie.
    pub fn kill(&self) -> Result<()> {
        let mut child = self
            .child
            .lock()
            .map_err(|e| AppError::General(format!("PTY child lock poisoned: {}", e)))?;
        if let Some(child) = child.as_mut() {
            if let Err(e) = child.kill() {
                tracing::warn!("Failed to kill PTY child {}: {}", self.id, e);
            }
            let _ = child.wait();
        }
        *child = None;
        Ok(())
    }

    /// True once the shell process has exited (either because the user typed
    /// `exit`/`Ctrl-D` or because the PTY master closed).
    pub fn is_alive(&self) -> bool {
        let mut child = match self.child.lock() {
            Ok(c) => c,
            Err(_) => return false,
        };
        match child.as_mut() {
            Some(child) => child.try_wait().map(|st| st.is_none()).unwrap_or(false),
            None => false,
        }
    }
}
