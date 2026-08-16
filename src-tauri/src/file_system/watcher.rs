use std::path::{Path, PathBuf};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use crate::error::{AppError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FsEvent {
    Created(PathBuf),
    Modified(PathBuf),
    Deleted(PathBuf),
}

/// A `notify`-based recursive file watcher. The callback runs on notify's
/// background thread and must be cheap (e.g. forwarding to a Tauri channel).
pub struct FileWatcher {
    watcher: RecommendedWatcher,
}

impl FileWatcher {
    /// Creates a watcher that invokes `on_event` (on notify's thread) for every
    /// created/modified/deleted path under a watched root.
    pub fn new<F>(on_event: F) -> Result<Self>
    where
        F: Fn(FsEvent) + Send + 'static,
    {
        let watcher = RecommendedWatcher::new(
            move |res: std::result::Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    for path in event.paths {
                        match event.kind {
                            EventKind::Create(_) => on_event(FsEvent::Created(path)),
                            EventKind::Modify(_) => on_event(FsEvent::Modified(path)),
                            EventKind::Remove(_) => on_event(FsEvent::Deleted(path)),
                            _ => {}
                        }
                    }
                }
            },
            Config::default(),
        )
        .map_err(|e| AppError::General(format!("Failed to create watcher: {}", e)))?;

        Ok(Self { watcher })
    }

    pub fn watch(&mut self, path: &Path) -> Result<()> {
        self.watcher
            .watch(path, RecursiveMode::Recursive)
            .map_err(|e| AppError::General(format!("Failed to watch path: {}", e)))?;
        tracing::info!("Watching path: {:?}", path);
        Ok(())
    }
}
