import { create } from 'zustand';
import { Channel } from '@tauri-apps/api/core';
import * as ipc from '../lib/ipc';
import { languageForPath } from '../lib/monaco';
import type { DirEntryItem, OpenTab } from '../types';

interface WorkspaceState {
  projectRoot: string | null;
  projectName: string | null;

  expandedDirs: Record<string, boolean>;
  dirEntries: Record<string, DirEntryItem[]>;
  loadingDirs: Record<string, boolean>;

  tabs: OpenTab[];
  activePath: string | null;
  reloadingFromDisk: Record<string, number>;

  pendingCursor: { path: string; line: number } | null;

  statusMessage: string;

  openProject: (path: string) => Promise<void>;
  closeProject: () => void;
  setStatus: (msg: string) => void;

  toggleDir: (path: string) => Promise<void>;
  refreshDir: (path: string) => Promise<void>;
  reloadAllExpanded: () => Promise<void>;

  openFile: (path: string, line?: number) => Promise<void>;
  closeTab: (path: string) => void;
  setActive: (path: string) => void;
  updateContent: (path: string, content: string) => void;
  saveFile: (path: string) => Promise<void>;
  applyExternalContent: (path: string, content: string) => void;
  revealCursor: () => { path: string; line: number } | null;
}

function nameOf(path: string): string {
  return path.split('/').pop() ?? path;
}

function parentOf(path: string): string {
  const idx = path.lastIndexOf('/');
  return idx > 0 ? path.slice(0, idx) : path;
}

export const useWorkspace = create<WorkspaceState>((set, get) => ({
  projectRoot: null,
  projectName: null,
  expandedDirs: {},
  dirEntries: {},
  loadingDirs: {},
  tabs: [],
  activePath: null,
  reloadingFromDisk: {},
  pendingCursor: null,
  statusMessage: '',

  setStatus: (msg) => set({ statusMessage: msg }),

  openProject: async (path) => {
    const info = await ipc.projectOpen(path);
    set({
      projectRoot: info.root,
      projectName: info.name,
      expandedDirs: { [info.root]: true },
      dirEntries: {},
      tabs: [],
      activePath: null,
    });
    await get().refreshDir(info.root);
    // Live-reload open tabs / the explorer when files change on disk.
    watchProject();
  },

  closeProject: () =>
    set({
      projectRoot: null,
      projectName: null,
      expandedDirs: {},
      dirEntries: {},
      tabs: [],
      activePath: null,
    }),

  refreshDir: async (path) => {
    set((s) => ({ loadingDirs: { ...s.loadingDirs, [path]: true } }));
    try {
      const entries = await ipc.fsListDir(path);
      const filtered = entries.filter(
        (e) => !e.name.startsWith('._') && e.name !== '.DS_Store',
      );
      set((s) => ({ dirEntries: { ...s.dirEntries, [path]: filtered } }));
    } catch (err) {
      console.error('list dir failed', path, err);
    } finally {
      set((s) => ({ loadingDirs: { ...s.loadingDirs, [path]: false } }));
    }
  },

  toggleDir: async (path) => {
    const { expandedDirs, dirEntries } = get();
    const isExpanded = !expandedDirs[path];
    set({ expandedDirs: { ...expandedDirs, [path]: isExpanded } });
    if (isExpanded && !dirEntries[path]) {
      await get().refreshDir(path);
    }
  },

  reloadAllExpanded: async () => {
    const { expandedDirs, projectRoot } = get();
    if (!projectRoot) return;
    const dirs = Object.keys(expandedDirs).filter((d) => expandedDirs[d]);
    if (!dirs.includes(projectRoot)) dirs.push(projectRoot);
    for (const dir of dirs) {
      await get().refreshDir(dir);
    }
  },

  openFile: async (path, line) => {
    const { tabs } = get();
    const existing = tabs.find((t) => t.path === path);
    if (!existing) {
      get().setStatus(`Opening ${nameOf(path)}…`);
      try {
        const payload = await ipc.fsReadFile(path);
        const tab: OpenTab = {
          path,
          name: nameOf(path),
          content: payload.content,
          savedContent: payload.content,
          language: languageForPath(path),
        };
        set((s) => ({ tabs: [...s.tabs, tab], activePath: path }));
      } catch (err) {
        get().setStatus(`Cannot open file: ${String(err)}`);
        return;
      }
    } else {
      set({ activePath: path });
    }
    if (line !== undefined && line >= 1) {
      set({ pendingCursor: { path, line } });
    }
    get().setStatus('');
  },

  closeTab: (path) => {
    const { tabs, activePath } = get();
    // Dirty-tab guard: never silently discard unsaved edits.
    const tab = tabs.find((t) => t.path === path);
    if (tab && tab.content !== tab.savedContent) {
      // The webview supports the native confirm dialog.
      const ok = window.confirm(`"${tab.name}" has unsaved changes. Close anyway?`);
      if (!ok) return;
    }
    const remaining = tabs.filter((t) => t.path !== path);
    let nextActive = activePath;
    if (activePath === path) {
      const idx = tabs.findIndex((t) => t.path === path);
      nextActive = remaining[Math.min(idx, remaining.length - 1)]?.path ?? null;
    }
    set({ tabs: remaining, activePath: nextActive });
  },

  setActive: (path) => set({ activePath: path }),

  updateContent: (path, content) =>
    set((s) => ({
      tabs: s.tabs.map((t) => (t.path === path ? { ...t, content } : t)),
    })),

  applyExternalContent: (path, content) =>
    set((s) => ({
      tabs: s.tabs.map((t) =>
        t.path === path ? { ...t, content, savedContent: content } : t,
      ),
      reloadingFromDisk: {
        ...s.reloadingFromDisk,
        [path]: (s.reloadingFromDisk[path] ?? 0) + 1,
      },
    })),

  saveFile: async (path) => {
    const tab = get().tabs.find((t) => t.path === path);
    if (!tab) return;
    try {
      await ipc.fsWriteFile(path, tab.content);
      set((s) => ({
        tabs: s.tabs.map((t) => (t.path === path ? { ...t, savedContent: tab.content } : t)),
        statusMessage: `Saved ${tab.name}`,
      }));
      setTimeout(() => {
        if (get().statusMessage === `Saved ${tab.name}`) set({ statusMessage: '' });
      }, 3000);
    } catch (err) {
      get().setStatus(`Save failed: ${String(err)}`);
    }
  },

  revealCursor: () => {
    const pending = get().pendingCursor;
    if (pending) set({ pendingCursor: null });
    return pending;
  },
}));

export function isDirty(tab: OpenTab): boolean {
  return tab.content !== tab.savedContent;
}

export async function reloadOpenTabsFromDisk(): Promise<void> {
  const { tabs, applyExternalContent, reloadAllExpanded } = useWorkspace.getState();
  for (const tab of tabs) {
    try {
      const payload = await ipc.fsReadFile(tab.path);
      const dirty = tab.content !== tab.savedContent;
      if (!dirty && payload.content !== tab.content) {
        applyExternalContent(tab.path, payload.content);
      }
    } catch {
      /* file may have been deleted by the agent */
    }
  }
  await reloadAllExpanded();
}

/**
 * Wires the backend file watcher so external edits (other tools, git checkouts,
 * the agent's `run_command`) reload open tabs and refresh the explorer live.
 * Returns a cleanup function.
 */
export function watchProject(): () => void {
  const channel = new Channel<ipc.FsEvent>();
  channel.onmessage = (event) => {
    if ('Modified' in event) {
      const raw = (event as { Modified: string | string[] }).Modified;
      const paths = Array.isArray(raw) ? raw : [raw];
      const { tabs } = useWorkspace.getState();
      for (const path of paths) {
        const tab = tabs.find((t) => t.path === path);
        if (!tab) continue;
        // Never clobber unsaved edits in the editor.
        if (tab.content !== tab.savedContent) continue;
        ipc
          .fsReadFile(path)
          .then((payload) => {
            if (payload.content !== tab.content) {
              useWorkspace.getState().applyExternalContent(path, payload.content);
            }
          })
          .catch(() => {});
      }
    }
    if ('Created' in event || 'Deleted' in event) {
      // Refresh the explorer tree so new/removed files show up live. Deleted
      // open tabs are intentionally kept (the dirty-guard may block closing,
      // and the content can still be recovered by saving).
      useWorkspace.getState().reloadAllExpanded();
    }
  };
  ipc.fsWatchStart(channel).catch(() => {
    /* backend not ready (no project yet) */
  });
  return () => {
    // The Tauri channel is auto-unregistered when dropped; nothing else to do.
  };
}

export { parentOf };
