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

// Module-scope in-flight registry for `openFile` (double-click dedupe).
const pendingOpens = new Set<string>();

// Active file-watcher cleanup (see openProject/closeProject).
let watcherCleanup: (() => void) | null = null;
function stopWatcher() {
  watcherCleanup?.();
  watcherCleanup = null;
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
    // Keep the cleanup so closeProject can stop the watcher — otherwise the
    // closed tree stayed watched until the NEXT open replaced the slot.
    stopWatcher();
    watcherCleanup = watchProject();
  },

  closeProject: () => {
    stopWatcher();
    set({
      projectRoot: null,
      projectName: null,
      expandedDirs: {},
      dirEntries: {},
      tabs: [],
      activePath: null,
    });
  },

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
      // In-flight dedupe: a fast double-click used to pass the `existing`
      // lookup twice before either fsReadFile resolved, appending duplicate
      // tabs (and duplicate React keys).
      if (pendingOpens.has(path)) return;
      pendingOpens.add(path);
      get().setStatus(`Opening ${nameOf(path)}…`);
      try {
        const payload = await ipc.fsReadFile(path);
        // Re-check after the await — the first invocation may have won.
        if (useWorkspace.getState().tabs.some((t) => t.path === path)) {
          set({ activePath: path });
          return;
        }
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
      } finally {
        pendingOpens.delete(path);
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
      // Prune counters for tabs that no longer exist — the map used to grow
      // forever across project switches (one key per externally-reloaded file).
      reloadingFromDisk: {
        ...Object.fromEntries(
          Object.entries(s.reloadingFromDisk).filter(([p]) =>
            s.tabs.some((t) => t.path === p),
          ),
        ),
        [path]: (s.reloadingFromDisk[path] ?? 0) + 1,
      },
    })),

  saveFile: async (path) => {
    const tab = get().tabs.find((t) => t.path === path);
    if (!tab) return;
    try {
      // Staleness precondition: hash of what this tab BELIEVES is on disk
      // (savedContent). If the agent or another process rewrote the file in
      // the meantime, the backend refuses the save instead of silently
      // destroying their work (lost-update guard).
      const expectedSha = await ipc.sha256Hex(tab.savedContent);
      const contentToWrite = tab.content;
      await ipc.fsWriteFile(path, contentToWrite, undefined, expectedSha);
      // Post-write reconcile: keystrokes that landed DURING the await must not
      // be marked as saved — keep the tab dirty so the next Cmd+S picks them up.
      set((s) => ({
        tabs: s.tabs.map((t) =>
          t.path === path && t.content === contentToWrite
            ? { ...t, savedContent: contentToWrite }
            : t,
        ),
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
  const { applyExternalContent, reloadAllExpanded } = useWorkspace.getState();
  for (const tab of useWorkspace.getState().tabs) {
    try {
      const payload = await ipc.fsReadFile(tab.path);
      // Re-check dirtiness AFTER the await against FRESH state: the user may
      // have typed while the file was being read. Applying based on the stale
      // pre-await snapshot silently clobbered those keystrokes.
      const fresh = useWorkspace.getState().tabs.find((t) => t.path === tab.path);
      if (!fresh) continue;
      const dirty = fresh.content !== fresh.savedContent;
      if (!dirty && payload.content !== fresh.content) {
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
      const raw: string = event.Modified;
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
            // Re-check dirtiness AFTER the await from fresh state: keystrokes
            // that landed during the read must win over disk content.
            const fresh = useWorkspace.getState().tabs.find((t) => t.path === path);
            if (!fresh || fresh.content !== fresh.savedContent) return;
            if (payload.content !== fresh.content) {
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
