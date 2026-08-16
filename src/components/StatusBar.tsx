import React from 'react';
import { GitBranch, ShieldCheck, TerminalSquare } from 'lucide-react';
import { useWorkspace } from '../store/workspace';
import { useLayout } from '../store/layout';

export const StatusBar: React.FC = () => {
  const statusMessage = useWorkspace((s) => s.statusMessage);
  const projectName = useWorkspace((s) => s.projectName);
  const tabs = useWorkspace((s) => s.tabs);
  const terminalOpen = useLayout((s) => s.terminalOpen);
  const toggleTerminal = useLayout((s) => s.toggleTerminal);

  return (
    <footer className="status-bar">
      <div className="status-left">
        <span className="status-item">
          <ShieldCheck size={18} className="icon-emerald" /> PathGuard active
        </span>
        <span className="status-item">
          <GitBranch size={18} /> {projectName ?? '—'}
        </span>
      </div>
      <div className="status-center">
        {statusMessage && <span className="status-message">{statusMessage}</span>}
      </div>
      <div className="status-right">
        <span className="status-item">{tabs.length} open</span>
        <button className={`status-item status-btn ${terminalOpen ? 'active' : ''}`} onClick={toggleTerminal}>
          <TerminalSquare size={18} /> Terminal
        </button>
        <span className="status-item">Rust Engine · Tauri v2</span>
      </div>
    </footer>
  );
};
