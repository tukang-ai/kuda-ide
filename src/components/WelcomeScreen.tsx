import React from 'react';
import { FolderOpen, Shield, Sparkles, Terminal } from 'lucide-react';
import { useWorkspace } from '../store/workspace';
import { openFolderDialog } from '../lib/ipc';

export const WelcomeScreen: React.FC = () => {
  const openProject = useWorkspace((s) => s.openProject);

  const browse = async () => {
    const selected = await openFolderDialog();
    if (typeof selected === 'string') {
      try {
        await openProject(selected);
      } catch (err) {
        alert(`Failed to open project: ${err}`);
      }
    }
  };

  return (
    <div className="welcome-screen">
      <div className="welcome-inner animate-slide-up">
        <div className="welcome-logo gradient-text">KUDA<span className="brand-rest">IDE</span></div>
        <div className="welcome-tagline">Agentic Hybrid IDE — Rust Native Engine</div>

        <button className="welcome-open-btn" onClick={browse}>
          <FolderOpen size={18} /> Open Project Folder
        </button>

        <div className="welcome-features">
          <div className="welcome-feature glass-panel">
            <Sparkles size={18} className="icon-accent" />
            <div>
              <div className="feature-title">Kuda Agent</div>
              <div className="feature-desc text-muted">Gemini-powered agent with multi-file tool calling and streaming.</div>
            </div>
          </div>
          <div className="welcome-feature glass-panel">
            <Shield size={18} className="icon-emerald" />
            <div>
              <div className="feature-title">PathGuard Security</div>
              <div className="feature-desc text-muted">Canonical path boundary enforcement + full-file checkpoints.</div>
            </div>
          </div>
          <div className="welcome-feature glass-panel">
            <Terminal size={18} className="icon-cyan" />
            <div>
              <div className="feature-title">Native PTY Terminal</div>
              <div className="feature-desc text-muted">Real shell sessions with xterm.js over portable-pty.</div>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};
