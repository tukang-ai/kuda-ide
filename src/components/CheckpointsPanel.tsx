import React, { useCallback, useEffect, useState } from 'react';
import { History, RotateCcw, Undo2 } from 'lucide-react';
import * as ipc from '../lib/ipc';
import { reloadOpenTabsFromDisk } from '../store/workspace';
import type { FileCheckpoint, SessionInfo } from '../types';

function fileName(path: string): string {
  return path.split('/').pop() ?? path;
}

export const CheckpointsPanel: React.FC = () => {
  const [checkpoints, setCheckpoints] = useState<FileCheckpoint[]>([]);
  const [sessions, setSessions] = useState<SessionInfo[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [list, sessionList] = await Promise.all([
        ipc.historyListCheckpoints(),
        ipc.historyListSessions(),
      ]);
      setCheckpoints(list);
      setSessions(sessionList);
    } catch (err) {
      setError(String(err));
      setCheckpoints([]);
      setSessions([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  const afterMutation = async () => {
    await reloadOpenTabsFromDisk();
    await load();
  };

  const earliestForFile = (filePath: string, sessionId: string): FileCheckpoint | undefined =>
    checkpoints
      .filter((c) => c.session_id === sessionId && c.original_file_path === filePath)
      .sort((a, b) => new Date(a.timestamp).getTime() - new Date(b.timestamp).getTime())[0];

  const restoreFile = async (checkpoint: FileCheckpoint) => {
    const verb = checkpoint.existed_before ? 'Restore' : 'Remove';
    if (!confirm(`${verb} "${fileName(checkpoint.original_file_path)}" to its state before the edit?`)) return;
    try {
      await ipc.historyRestoreCheckpoint(checkpoint.checkpoint_id);
      await afterMutation();
    } catch (err) {
      alert(`Restore failed: ${err}`);
    }
  };

  const revertSession = async (session: SessionInfo) => {
    if (!confirm(
      `Revert ${session.files.length} file(s) edited in the run from ${new Date(session.timestamp).toLocaleString()}?\n` +
      `Modified files are fully restored, created files removed, deleted files restored.`,
    )) return;
    try {
      await ipc.historyRevertSession(session.session_id);
      await afterMutation();
    } catch (err) {
      alert(`Revert failed: ${err}`);
    }
  };

  const ungrouped = checkpoints.filter((c) => !c.session_id);

  return (
    <div className="sidebar-content">
      <div className="sidebar-header">
        <span className="sidebar-title">
          <History size={13} style={{ verticalAlign: '-2px' }} /> Checkpoints
        </span>
        <button className="icon-btn" onClick={load} title="Refresh">{loading ? '…' : '⟳'}</button>
      </div>
      <div className="tree-container">
        {error && <div className="agent-error">{error}</div>}
        {sessions.length === 0 && ungrouped.length === 0 && !loading && !error && (
          <div className="text-muted search-empty">
            No checkpoints yet. A full-file checkpoint is created automatically before every edit.
          </div>
        )}

        {sessions.map((session) => (
          <div key={session.session_id} className="session-group">
            <div className="session-header">
              <div className="session-title">
                <Undo2 size={12} />
                <span>Agent run · {session.files.length} file(s)</span>
              </div>
              <div className="session-meta">
                {new Date(session.timestamp).toLocaleString()}
              </div>
              <button className="icon-btn session-revert" title="Revert all files in this run" onClick={() => revertSession(session)}>
                <RotateCcw size={13} className="icon-accent" />
              </button>
            </div>
            {session.files.map((file) => {
              const checkpoint = earliestForFile(file, session.session_id);
              return (
                <div key={file} className="checkpoint-row">
                  <div className="checkpoint-info">
                    <div className="checkpoint-file" title={file}>{fileName(file)}</div>
                    <div className="checkpoint-meta">
                      {checkpoint?.existed_before ? 'restored on revert' : 'created (removed on revert)'}
                    </div>
                  </div>
                  {checkpoint && (
                    <button className="icon-btn" title={checkpoint.existed_before ? 'Restore this file' : 'Remove this created file'} onClick={() => restoreFile(checkpoint)}>
                      <RotateCcw size={13} className="icon-accent" />
                    </button>
                  )}
                </div>
              );
            })}
          </div>
        ))}

        {ungrouped.length > 0 && (
          <div className="session-group">
            <div className="session-header">
              <div className="session-title"><Undo2 size={12} /><span>Manual edits</span></div>
            </div>
            {ungrouped.map((c) => (
              <div key={c.checkpoint_id} className="checkpoint-row">
                <div className="checkpoint-info">
                  <div className="checkpoint-file" title={c.original_file_path}>{fileName(c.original_file_path)}</div>
                  <div className="checkpoint-meta">{new Date(c.timestamp).toLocaleString()}</div>
                </div>
                <button className="icon-btn" title="Restore this checkpoint" onClick={() => restoreFile(c)}>
                  <RotateCcw size={13} className="icon-accent" />
                </button>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
};
