import React from 'react';
import {
  FolderOpen, PanelBottomClose, PanelLeftClose, PanelRightClose, Search, Settings,
} from 'lucide-react';
import { useWorkspace } from '../store/workspace';
import { useAgent } from '../store/agent';
import { useLayout } from '../store/layout';
import { openFolderDialog } from '../lib/ipc';

export const TitleBar: React.FC = () => {
  const projectName = useWorkspace((s) => s.projectName);
  const projectRoot = useWorkspace((s) => s.projectRoot);
  const openProject = useWorkspace((s) => s.openProject);
  const activePath = useWorkspace((s) => s.activePath);
  const hasGeminiKey = useAgent((s) => s.hasGeminiKey);
  const hasHubKey = useAgent((s) => s.hasHubKey);
  const hasCustomProvider = useAgent((s) => s.hasCustomProvider);

  const sidebarOpen = useLayout((s) => s.sidebarOpen);
  const terminalOpen = useLayout((s) => s.terminalOpen);
  const agentOpen = useLayout((s) => s.agentOpen);
  const toggleSidebar = useLayout((s) => s.toggleSidebar);
  const toggleTerminal = useLayout((s) => s.toggleTerminal);
  const toggleAgent = useLayout((s) => s.toggleAgent);
  const setSidebarView = useLayout((s) => s.setSidebarView);
  const setSettingsOpen = useLayout((s) => s.setSettingsOpen);

  const breadcrumbs = activePath && projectRoot
    ? activePath.startsWith(projectRoot)
      ? activePath.slice(projectRoot.length + 1).split('/')
      : activePath.split('/')
    : [];

  const browseFolder = async () => {
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
    <header className="title-bar">
      <div className="title-bar-left">
        <span className="brand gradient-text">KUDA<span className="brand-rest">IDE</span></span>
        <span className="version-badge">v0.1.0</span>
        <button className="title-btn" onClick={browseFolder}>
          <FolderOpen size={13} /> Open Folder
        </button>
      </div>

      <div className="title-bar-center">
        {projectName ? (
          <>
            <span>{projectName}</span>
            {breadcrumbs.map((crumb, i) => (
              <React.Fragment key={i}>
                <span className="crumb-sep">/</span>
                <span className={i === breadcrumbs.length - 1 ? 'crumb-active' : ''}>{crumb}</span>
              </React.Fragment>
            ))}
          </>
        ) : (
          <span className="text-muted">No folder open</span>
        )}
      </div>

      <div className="title-bar-right">
        {/* Antigravity Panel Layout Toggles Group */}
        <div className="layout-toggles-group">
          <button
            className={`icon-btn ${sidebarOpen ? 'toggled' : ''}`}
            title="Toggle Left Sidebar (Cmd+B)"
            onClick={toggleSidebar}
          >
            <PanelLeftClose size={15} />
          </button>
          <button
            className={`icon-btn ${terminalOpen ? 'toggled' : ''}`}
            title="Toggle Bottom Terminal (Cmd+J)"
            onClick={toggleTerminal}
          >
            <PanelBottomClose size={15} />
          </button>
          <button
            className={`icon-btn ${agentOpen ? 'toggled' : ''}`}
            title="Toggle AI Agent Panel (Cmd+I)"
            onClick={toggleAgent}
          >
            <PanelRightClose size={15} />
          </button>
        </div>

        <button
          className="icon-btn"
          title="Search Code (Cmd+F)"
          onClick={() => setSidebarView('search')}
        >
          <Search size={15} />
        </button>

        <div className={`model-badge glass-panel ${hasGeminiKey ? '' : 'no-key'}`}>
          <span className={`model-dot ${hasGeminiKey ? 'online' : 'offline'}`} />
          <span>{hasHubKey ? 'Kuda Hub' : hasCustomProvider ? 'Provider' : hasGeminiKey ? 'Gemini' : 'No API key'}</span>
        </div>
        <button className="icon-btn" title="Settings" onClick={() => setSettingsOpen(true)}>
          <Settings size={15} />
        </button>
      </div>
    </header>
  );
};
