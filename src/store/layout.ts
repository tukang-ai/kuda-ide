import { create } from 'zustand';

export type SidebarView = 'explorer' | 'search' | 'outline' | 'history';

const LAYOUT_KEY = 'kuda-ide.layout.v1';

interface PersistedLayout {
  sidebarView: SidebarView;
  sidebarOpen: boolean;
  terminalOpen: boolean;
  agentOpen: boolean;
}

function loadPersisted(): Partial<PersistedLayout> {
  try {
    const raw = localStorage.getItem(LAYOUT_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw) as Partial<PersistedLayout>;
    return parsed && typeof parsed === 'object' ? parsed : {};
  } catch {
    return {};
  }
}

function isSidebarView(v: unknown): v is SidebarView {
  return v === 'explorer' || v === 'search' || v === 'outline' || v === 'history';
}

interface LayoutState extends PersistedLayout {
  settingsOpen: boolean;

  setSidebarView: (view: SidebarView) => void;
  toggleSidebar: () => void;
  toggleTerminal: () => void;
  setTerminalOpen: (open: boolean) => void;
  toggleAgent: () => void;
  setSettingsOpen: (open: boolean) => void;
}

const persisted = loadPersisted();

export const useLayout = create<LayoutState>((set, get) => {
  // Persist every toggle so the IDE restores its panel layout across restarts
  // (previously the sidebar/terminal/agent open state always reset on launch).
  const persist = (next: Partial<PersistedLayout>) => {
    set((s) => {
      const merged = { ...s, ...next };
      try {
        localStorage.setItem(
          LAYOUT_KEY,
          JSON.stringify({
            sidebarView: merged.sidebarView,
            sidebarOpen: merged.sidebarOpen,
            terminalOpen: merged.terminalOpen,
            agentOpen: merged.agentOpen,
          }),
        );
      } catch {
        /* storage unavailable */
      }
      return merged;
    });
  };

  return {
    sidebarView: isSidebarView(persisted.sidebarView) ? persisted.sidebarView : 'explorer',
    sidebarOpen: persisted.sidebarOpen ?? true,
    terminalOpen: persisted.terminalOpen ?? false,
    agentOpen: persisted.agentOpen ?? true,
    settingsOpen: false,

    setSidebarView: (view) => {
      const { sidebarView, sidebarOpen } = get();
      if (view === sidebarView) {
        persist({ sidebarOpen: !sidebarOpen });
      } else {
        persist({ sidebarView: view, sidebarOpen: true });
      }
    },
    toggleSidebar: () => persist({ sidebarOpen: !get().sidebarOpen }),
    toggleTerminal: () => persist({ terminalOpen: !get().terminalOpen }),
    setTerminalOpen: (open) => persist({ terminalOpen: open }),
    toggleAgent: () => persist({ agentOpen: !get().agentOpen }),
    setSettingsOpen: (open) => set({ settingsOpen: open }),
  };
});
