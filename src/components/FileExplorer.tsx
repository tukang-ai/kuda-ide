import React, { useState, useEffect, useRef } from 'react';
import {
  ChevronDown, ChevronRight, File, FileCode, FileJson, FileText, Folder, FolderOpen,
  FilePlus, FolderPlus, RefreshCw, Pencil, Trash2, Copy, Clipboard, Files, Link,
} from 'lucide-react';
import { useWorkspace } from '../store/workspace';
import * as ipc from '../lib/ipc';
import type { DirEntryItem } from '../types';

function fileIcon(name: string) {
  const ext = name.includes('.') ? name.split('.').pop()!.toLowerCase() : '';
  const lower = name.toLowerCase();

  if (lower === 'cargo.toml' || lower === 'cargo.lock') return <FileCode size={14} style={{ color: '#f97316' }} />;
  if (lower === 'package.json') return <FileJson size={14} style={{ color: '#38bdf8' }} />;
  if (lower === 'dockerfile') return <FileCode size={14} style={{ color: '#0ea5e9' }} />;
  if (lower.startsWith('.git')) return <FileText size={14} style={{ color: '#f43f5e' }} />;

  switch (ext) {
    case 'rs':
      return <FileCode size={14} style={{ color: '#f97316' }} />;
    case 'ts':
    case 'tsx':
      return <FileCode size={14} style={{ color: '#38bdf8' }} />;
    case 'js':
    case 'jsx':
    case 'mjs':
      return <FileCode size={14} style={{ color: '#facc15' }} />;
    case 'py':
      return <FileCode size={14} style={{ color: '#3b82f6' }} />;
    case 'go':
      return <FileCode size={14} style={{ color: '#06b6d4' }} />;
    case 'css':
    case 'scss':
    case 'less':
      return <FileCode size={14} style={{ color: '#ec4899' }} />;
    case 'html':
      return <FileCode size={14} style={{ color: '#f97316' }} />;
    case 'json':
    case 'toml':
    case 'yaml':
    case 'yml':
      return <FileJson size={14} style={{ color: '#fbbf24' }} />;
    case 'md':
    case 'markdown':
      return <FileText size={14} style={{ color: '#a855f7' }} />;
    case 'sh':
    case 'bash':
    case 'zsh':
      return <FileCode size={14} style={{ color: '#10b981' }} />;
    default:
      return <File size={14} className="icon-muted" />;
  }
}

interface CreatingState {
  parentPath: string;
  isDir: boolean;
}

interface RenamingState {
  path: string;
  oldName: string;
  parentPath: string;
}

interface ContextMenuState {
  x: number;
  y: number;
  entry: DirEntryItem;
}

export const FileExplorer: React.FC = () => {
  // Narrow selectors: the previous bare `useWorkspace()` subscribed to the
  // WHOLE store, so every editor keystroke (updateContent → tabs change)
  // re-rendered the entire recursive tree.
  const projectRoot = useWorkspace((s) => s.projectRoot);
  const projectName = useWorkspace((s) => s.projectName);
  const expandedDirs = useWorkspace((s) => s.expandedDirs);
  const dirEntries = useWorkspace((s) => s.dirEntries);
  const loadingDirs = useWorkspace((s) => s.loadingDirs);
  const activePath = useWorkspace((s) => s.activePath);
  const toggleDir = useWorkspace((s) => s.toggleDir);
  const refreshDir = useWorkspace((s) => s.refreshDir);
  const reloadAllExpanded = useWorkspace((s) => s.reloadAllExpanded);
  const openFile = useWorkspace((s) => s.openFile);
  const closeTab = useWorkspace((s) => s.closeTab);
  const setActive = useWorkspace((s) => s.setActive);
  const [creating, setCreating] = useState<CreatingState | null>(null);
  const [newName, setNewName] = useState('');
  const [renaming, setRenaming] = useState<RenamingState | null>(null);
  const [renameValue, setRenameValue] = useState('');
  const [draggedItem, setDraggedItem] = useState<DirEntryItem | null>(null);
  const [dragOverTarget, setDragOverTarget] = useState<string | null>(null);
  const [copiedItem, setCopiedItem] = useState<DirEntryItem | null>(null);
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null);

  const contextMenuRef = useRef<HTMLDivElement>(null);
  const renameInputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const handleClickOutside = (e: MouseEvent) => {
      if (contextMenuRef.current && !contextMenuRef.current.contains(e.target as Node)) {
        setContextMenu(null);
      }
    };
    window.addEventListener('mousedown', handleClickOutside);
    return () => window.removeEventListener('mousedown', handleClickOutside);
  }, []);

  useEffect(() => {
    if (renaming && renameInputRef.current) {
      const input = renameInputRef.current;
      input.focus();
      const dotIdx = renaming.oldName.lastIndexOf('.');
      if (dotIdx > 0) {
        input.setSelectionRange(0, dotIdx);
      } else {
        input.select();
      }
    }
  }, [renaming]);

  if (!projectRoot) return null;

  const submitCreate = async () => {
    if (!creating || !newName.trim()) {
      setCreating(null);
      return;
    }
    const fullPath = `${creating.parentPath}/${newName.trim()}`;
    try {
      if (creating.isDir) {
        await ipc.fsCreateDir(fullPath);
      } else {
        await ipc.fsWriteFile(fullPath, '');
      }
      if (!expandedDirs[creating.parentPath]) {
        await toggleDir(creating.parentPath);
      }
      await refreshDir(creating.parentPath);
    } catch (err) {
      alert(`Create failed: ${err}`);
    }
    setCreating(null);
    setNewName('');
  };

  const startRename = (entry: DirEntryItem) => {
    const parent = entry.path.slice(0, entry.path.lastIndexOf('/')) || projectRoot;
    setRenaming({
      path: entry.path,
      oldName: entry.name,
      parentPath: parent,
    });
    setRenameValue(entry.name);
  };

  const submitRename = async () => {
    if (!renaming || !renameValue.trim() || renameValue.trim() === renaming.oldName) {
      setRenaming(null);
      return;
    }
    const newPath = `${renaming.parentPath}/${renameValue.trim()}`;
    // Resolve the unsaved-changes question BEFORE touching disk: after the
    // rename completes, cancelling the confirm used to leave a tab pointing
    // at the old path whose save would resurrect a stale file.
    const tab = useWorkspace.getState().tabs.find((t) => t.path === renaming.path);
    if (tab && tab.content !== tab.savedContent) {
      if (
        !confirm(`"${renaming.oldName}" has unsaved changes. Rename anyway and discard them?`)
      ) {
        setRenaming(null);
        return;
      }
    }
    try {
      await ipc.fsRename(renaming.path, newPath);
      await refreshDir(renaming.parentPath);
      closeTab(renaming.path);
    } catch (err) {
      alert(`Rename failed: ${err}`);
    }
    setRenaming(null);
  };

  const handleDelete = async (entry: DirEntryItem) => {
    if (!confirm(`Move "${entry.name}" to Trash?`)) return;
    // Same ordering as rename: settle dirty-tab state before the destructive
    // disk operation, not after.
    const tab = useWorkspace.getState().tabs.find((t) => t.path === entry.path);
    if (tab && tab.content !== tab.savedContent) {
      if (!confirm(`"${entry.name}" has unsaved changes. Delete anyway?`)) return;
    }
    try {
      await ipc.fsDelete(entry.path);
      const parent = entry.path.slice(0, entry.path.lastIndexOf('/')) || projectRoot;
      await refreshDir(parent);
      closeTab(entry.path);
    } catch (err) {
      alert(`Delete failed: ${err}`);
    }
  };

  const handleCopy = (entry: DirEntryItem) => {
    setCopiedItem(entry);
  };

  const handleCopyPath = (entry: DirEntryItem) => {
    navigator.clipboard.writeText(entry.path);
  };

  const handleDuplicate = async (entry: DirEntryItem) => {
    if (entry.is_dir) {
      alert('Duplicating directories is not supported directly.');
      return;
    }
    const parent = entry.path.slice(0, entry.path.lastIndexOf('/')) || projectRoot;
    const dotIdx = entry.name.lastIndexOf('.');
    const baseName = dotIdx !== -1 ? entry.name.slice(0, dotIdx) : entry.name;
    const ext = dotIdx !== -1 ? entry.name.slice(dotIdx) : '';
    const newPath = `${parent}/${baseName}_copy${ext}`;
    try {
      const payload = await ipc.fsReadFile(entry.path);
      await ipc.fsWriteFile(newPath, payload.content || '');
      await refreshDir(parent);
    } catch (err) {
      alert(`Duplicate failed: ${err}`);
    }
  };

  const handlePaste = async (targetFolderPath: string) => {
    if (!copiedItem) return;
    if (copiedItem.is_dir) {
      alert('Pasting directories is not supported directly.');
      return;
    }
    const newPath = `${targetFolderPath}/${copiedItem.name}`;
    try {
      const payload = await ipc.fsReadFile(copiedItem.path);
      await ipc.fsWriteFile(newPath, payload.content || '');
      await refreshDir(targetFolderPath);
    } catch (err) {
      alert(`Paste failed: ${err}`);
    }
  };

  const handleDragStart = (e: React.DragEvent, entry: DirEntryItem) => {
    setDraggedItem(entry);
    e.dataTransfer.setData('text/plain', entry.path);
    e.dataTransfer.setData('text/uri-list', `file://${entry.path}`);
    e.dataTransfer.effectAllowed = 'copyMove';
  };

  const handleDragOver = (e: React.DragEvent, targetPath: string, isDir: boolean) => {
    if (!isDir) return;
    e.preventDefault();
    e.stopPropagation();
    e.dataTransfer.dropEffect = 'move';
    if (dragOverTarget !== targetPath) {
      setDragOverTarget(targetPath);
    }
  };

  const handleDragLeave = (e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
  };

  const handleDrop = async (e: React.DragEvent, targetFolderPath: string) => {
    e.preventDefault();
    e.stopPropagation();
    setDragOverTarget(null);

    let sourcePath = e.dataTransfer.getData('text/plain') || draggedItem?.path;
    if (!sourcePath) {
      const uriList = e.dataTransfer.getData('text/uri-list');
      if (uriList) {
        sourcePath = uriList.replace(/^file:\/\//, '');
      }
    }
    if (!sourcePath) return;

    const cleanSource = sourcePath.trim();
    const cleanTarget = targetFolderPath.trim();
    const sourceFileName = cleanSource.slice(cleanSource.lastIndexOf('/') + 1);
    const destPath = `${cleanTarget}/${sourceFileName}`;

    if (cleanSource === destPath || cleanTarget.startsWith(cleanSource + '/')) {
      return;
    }

    const sourceParent = cleanSource.slice(0, cleanSource.lastIndexOf('/')) || projectRoot;

    try {
      await ipc.fsRename(cleanSource, destPath);
      await refreshDir(sourceParent);
      await refreshDir(cleanTarget);
      if (!expandedDirs[cleanTarget]) {
        await toggleDir(cleanTarget);
      }
      closeTab(cleanSource);
    } catch (err) {
      alert(`Move failed: ${err}`);
    } finally {
      setDraggedItem(null);
    }
  };

  const onContextMenuHandler = (e: React.MouseEvent, entry: DirEntryItem) => {
    e.preventDefault();
    e.stopPropagation();
    setActive(entry.path);
    setContextMenu({
      x: e.clientX,
      y: e.clientY,
      entry,
    });
  };

  const handleMenuItemAction = (action: () => void) => (e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    setContextMenu(null);
    action();
  };

  const findEntryByPath = (path: string): DirEntryItem | null => {
    for (const p in dirEntries) {
      const found = dirEntries[p]?.find((e) => e.path === path);
      if (found) return found;
    }
    return null;
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (renaming || creating) return;
    if (!activePath) return;

    const currentEntry = findEntryByPath(activePath);
    if (!currentEntry) return;

    if (e.key === 'F2') {
      e.preventDefault();
      startRename(currentEntry);
    } else if (e.key === 'Delete' || (e.key === 'Backspace' && (e.metaKey || e.ctrlKey))) {
      e.preventDefault();
      handleDelete(currentEntry);
    } else if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'c') {
      e.preventDefault();
      handleCopy(currentEntry);
    } else if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'd') {
      e.preventDefault();
      handleDuplicate(currentEntry);
    } else if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'v') {
      e.preventDefault();
      const targetFolder = currentEntry.is_dir ? currentEntry.path : currentEntry.path.slice(0, currentEntry.path.lastIndexOf('/'));
      handlePaste(targetFolder);
    }
  };

  const renderEntries = (parentPath: string, depth: number): React.ReactNode => {
    const entries = dirEntries[parentPath];
    if (!entries) {
      return loadingDirs[parentPath] ? (
        <div className="tree-row" style={{ paddingLeft: depth * 14 + 10 }}>
          <span className="text-muted">Loading…</span>
        </div>
      ) : null;
    }

    const nodes: React.ReactNode[] = [];

    if (creating && creating.parentPath === parentPath) {
      nodes.push(
        <div key="__creating" className="tree-row" style={{ paddingLeft: depth * 14 + 10 }}>
          {creating.isDir ? <Folder size={14} /> : <File size={14} />}
          <input
            autoFocus
            className="tree-input"
            value={newName}
            onChange={(e) => setNewName(e.target.value)}
            onBlur={submitCreate}
            onKeyDown={(e) => {
              if (e.key === 'Enter') submitCreate();
              if (e.key === 'Escape') { setCreating(null); setNewName(''); }
            }}
          />
        </div>,
      );
    }

    for (const entry of entries) {
      const isRenaming = renaming?.path === entry.path;

      if (entry.is_dir) {
        const expanded = !!expandedDirs[entry.path];
        const isDragOver = dragOverTarget === entry.path;
        nodes.push(
          <div
            key={entry.path}
            draggable={!isRenaming}
            onDragStart={(e) => handleDragStart(e, entry)}
            onDragOver={(e) => handleDragOver(e, entry.path, true)}
            onDragLeave={handleDragLeave}
            onDrop={(e) => handleDrop(e, entry.path)}
            onContextMenu={(e) => onContextMenuHandler(e, entry)}
            className={`tree-row ${activePath === entry.path ? 'active' : ''} ${isDragOver ? 'drag-over' : ''}`}
            style={{ paddingLeft: depth * 14 + 8 }}
            onClick={() => setActive(entry.path)}
            onDoubleClick={() => toggleDir(entry.path)}
            title={entry.path}
          >
            {expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
            {expanded ? <FolderOpen size={14} className="icon-folder" /> : <Folder size={14} className="icon-folder" />}
            {isRenaming ? (
              <input
                ref={renameInputRef}
                className="tree-input"
                value={renameValue}
                onChange={(e) => setRenameValue(e.target.value)}
                onBlur={submitRename}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') submitRename();
                  if (e.key === 'Escape') setRenaming(null);
                }}
                onClick={(e) => e.stopPropagation()}
              />
            ) : (
              <span className="tree-name">{entry.name}</span>
            )}
          </div>,
        );
        if (expanded) {
          nodes.push(<div key={`${entry.path}::children`}>{renderEntries(entry.path, depth + 1)}</div>);
        }
      } else {
        nodes.push(
          <div
            key={entry.path}
            draggable={!isRenaming}
            onDragStart={(e) => handleDragStart(e, entry)}
            onContextMenu={(e) => onContextMenuHandler(e, entry)}
            className={`tree-row ${activePath === entry.path ? 'active' : ''}`}
            style={{ paddingLeft: depth * 14 + 22 }}
            onClick={() => setActive(entry.path)}
            onDoubleClick={() => openFile(entry.path)}
            title={entry.path}
          >
            {fileIcon(entry.name)}
            {isRenaming ? (
              <input
                ref={renameInputRef}
                className="tree-input"
                value={renameValue}
                onChange={(e) => setRenameValue(e.target.value)}
                onBlur={submitRename}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') submitRename();
                  if (e.key === 'Escape') setRenaming(null);
                }}
                onClick={(e) => e.stopPropagation()}
              />
            ) : (
              <span className="tree-name">{entry.name}</span>
            )}
          </div>,
        );
      }
    }

    if (entries.length === 0 && !(creating && creating.parentPath === parentPath)) {
      nodes.push(
        <div key="__empty" className="tree-row" style={{ paddingLeft: depth * 14 + 22 }}>
          <span className="text-muted">Empty</span>
        </div>,
      );
    }

    return nodes;
  };

  const isRootDragOver = dragOverTarget === projectRoot;

  return (
    <div className="sidebar-content" tabIndex={0} onKeyDown={handleKeyDown} style={{ outline: 'none' }}>
      <div className="sidebar-header">
        <span className="sidebar-title">{projectName}</span>
        <span className="sidebar-tools">
          <button className="icon-btn" title="New file" onClick={() => { setCreating({ parentPath: projectRoot, isDir: false }); setNewName(''); }}>
            <FilePlus size={14} />
          </button>
          <button className="icon-btn" title="New folder" onClick={() => { setCreating({ parentPath: projectRoot, isDir: true }); setNewName(''); }}>
            <FolderPlus size={14} />
          </button>
          <button className="icon-btn" title="Refresh" onClick={() => reloadAllExpanded()}>
            <RefreshCw size={14} />
          </button>
        </span>
      </div>
      <div
        className={`tree-container ${isRootDragOver ? 'drag-over' : ''}`}
        onDragOver={(e) => handleDragOver(e, projectRoot, true)}
        onDragLeave={handleDragLeave}
        onDrop={(e) => handleDrop(e, projectRoot)}
      >
        {renderEntries(projectRoot, 0)}
      </div>

      {contextMenu && (
        <div
          ref={contextMenuRef}
          className="context-menu"
          style={{ top: contextMenu.y, left: contextMenu.x }}
          onMouseDown={(e) => e.stopPropagation()}
          onClick={(e) => e.stopPropagation()}
        >
          {contextMenu.entry.is_dir && (
            <>
              <div className="context-menu-item" onClick={handleMenuItemAction(() => setCreating({ parentPath: contextMenu.entry.path, isDir: false }))}>
                <FilePlus size={13} /> New File...
              </div>
              <div className="context-menu-item" onClick={handleMenuItemAction(() => setCreating({ parentPath: contextMenu.entry.path, isDir: true }))}>
                <FolderPlus size={13} /> New Folder...
              </div>
              {copiedItem && (
                <div className="context-menu-item" onClick={handleMenuItemAction(() => handlePaste(contextMenu.entry.path))}>
                  <Clipboard size={13} /> Paste ({copiedItem.name}) <span className="menu-badge">⌘V</span>
                </div>
              )}
              <div className="context-menu-divider" />
            </>
          )}
          {!contextMenu.entry.is_dir && (
            <>
              <div className="context-menu-item" onClick={handleMenuItemAction(() => handleCopy(contextMenu.entry))}>
                <Copy size={13} /> Copy File <span className="menu-badge">⌘C</span>
              </div>
              <div className="context-menu-item" onClick={handleMenuItemAction(() => handleDuplicate(contextMenu.entry))}>
                <Files size={13} /> Duplicate <span className="menu-badge">⌘D</span>
              </div>
              <div className="context-menu-divider" />
            </>
          )}
          <div className="context-menu-item" onClick={handleMenuItemAction(() => handleCopyPath(contextMenu.entry))}>
            <Link size={13} /> Copy Path
          </div>
          <div className="context-menu-item" onClick={handleMenuItemAction(() => startRename(contextMenu.entry))}>
            <Pencil size={13} /> Rename... <span className="menu-badge">F2</span>
          </div>
          <div className="context-menu-divider" />
          <div className="context-menu-item danger" onClick={handleMenuItemAction(() => handleDelete(contextMenu.entry))}>
            <Trash2 size={13} /> Move to Trash <span className="menu-badge">⌘⌫</span>
          </div>
        </div>
      )}
    </div>
  );
};
