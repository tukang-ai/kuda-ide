import React, { useEffect } from 'react';
import { Panel, PanelGroup, PanelResizeHandle } from 'react-resizable-panels';
import { TitleBar } from './components/TitleBar';
import { ActivityBar } from './components/ActivityBar';
import { FileExplorer } from './components/FileExplorer';
import { SearchPanel } from './components/SearchPanel';
import { OutlinePanel } from './components/OutlinePanel';
import { CheckpointsPanel } from './components/CheckpointsPanel';
import { EditorPane } from './components/EditorPane';
import { TerminalPanel } from './components/TerminalPanel';
import { AgentPanel } from './components/AgentPanel';
import { StatusBar } from './components/StatusBar';
import { SettingsModal } from './components/SettingsModal';
import { WelcomeScreen } from './components/WelcomeScreen';
import { PendingExternalAccessList } from './components/AgentPanel';
import { useWorkspace } from './store/workspace';
import { useAgent } from './store/agent';
import { useLayout } from './store/layout';
import './lib/monaco';

const Sidebar: React.FC = () => {
  const view = useLayout((s) => s.sidebarView);
  switch (view) {
    case 'search':
      return <SearchPanel />;
    case 'outline':
      return <OutlinePanel />;
    case 'history':
      return <CheckpointsPanel />;
    default:
      return <FileExplorer />;
  }
};

export const App: React.FC = () => {
  const projectRoot = useWorkspace((s) => s.projectRoot);
  const sidebarOpen = useLayout((s) => s.sidebarOpen);
  const terminalOpen = useLayout((s) => s.terminalOpen);
  const agentOpen = useLayout((s) => s.agentOpen);
  const toggleTerminal = useLayout((s) => s.toggleTerminal);
  const toggleSidebar = useLayout((s) => s.toggleSidebar);
  const toggleAgent = useLayout((s) => s.toggleAgent);
  const pendingExternalRequests = useAgent((s) => s.pendingExternalRequests);
  const approveExternalAccess = useAgent((s) => s.approveExternalAccess);
  const denyExternalAccess = useAgent((s) => s.denyExternalAccess);
  const bindExternalEvents = useAgent((s) => s.bindExternalEvents);
  const initAgent = useAgent((s) => s.init);

  useEffect(() => {
    // App-wide channel so filesystem commands can surface Allow/Deny
    // notifications for out-of-project access even before the agent runs.
    bindExternalEvents();
    // Kick off key-status probing + session refresh timers at startup,
    // regardless of whether the Agent panel is open (init() used to live only
    // inside AgentPanel, so the badge stayed "No API key" until that panel
    // mounted). init() is idempotent (shared timer), so the AgentPanel call
    // may run again later without side effects.
    initAgent();
  }, [bindExternalEvents, initAgent]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;
      if (!mod) return;
      const target = e.target as HTMLElement | null;
      // Don't hijack shortcuts typed inside text fields / Monaco: Cmd+I is the
      // native italic binding in editors, and swallowing keystrokes mid-edit
      // made panel toggles fire unintentionally while typing.
      if (
        target &&
        (target.tagName === 'INPUT' ||
          target.tagName === 'TEXTAREA' ||
          target.isContentEditable ||
          target.closest('.monaco-editor'))
      ) {
        return;
      }
      const key = e.key.toLowerCase();
      if (key === 'j') {
        e.preventDefault();
        toggleTerminal();
      } else if (key === 'b') {
        e.preventDefault();
        toggleSidebar();
      } else if (key === 'i') {
        e.preventDefault();
        toggleAgent();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [toggleTerminal, toggleSidebar, toggleAgent]);

  return (
    <div className="app-root">
      <TitleBar />

      {!projectRoot ? (
        <WelcomeScreen />
      ) : (
        <div className="main-area">
          <ActivityBar />

          <PanelGroup direction="horizontal">
            {sidebarOpen && (
              <>
                <Panel id="sidebar" order={1} defaultSize={20} minSize={14} maxSize={40}>
                  <aside className="sidebar">
                    <Sidebar />
                  </aside>
                </Panel>
                <PanelResizeHandle className="resize-handle" />
              </>
            )}

            <Panel id="center" order={2}>
              <div className="center-column">
                <PanelGroup direction="vertical">
                  <Panel id="editor" order={1}>
                    <EditorPane />
                  </Panel>
                  {terminalOpen && (
                    <>
                      <PanelResizeHandle className="resize-handle" />
                      <Panel id="terminal" order={2} defaultSize={30} minSize={12}>
                        <TerminalPanel />
                      </Panel>
                    </>
                  )}
                </PanelGroup>
              </div>
            </Panel>

            {agentOpen && (
              <>
                <PanelResizeHandle className="resize-handle" />
                <Panel id="agent" order={3} defaultSize={30} minSize={20} maxSize={50}>
                  <aside className="agent-aside">
                    <AgentPanel />
                  </aside>
                </Panel>
              </>
            )}
          </PanelGroup>
        </div>
      )}

      <StatusBar />

      {pendingExternalRequests.length > 0 && (
        <div className="external-access-overlay">
          <PendingExternalAccessList
            requests={pendingExternalRequests}
            onAllow={(id) => approveExternalAccess(id)}
            onDeny={(id) => denyExternalAccess(id)}
          />
        </div>
      )}

      <SettingsModal />
    </div>
  );
};
