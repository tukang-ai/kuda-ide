import React, { useEffect, useRef } from 'react';
import { X, Circle } from 'lucide-react';
import { monaco } from '../lib/monaco';
import { useWorkspace, isDirty } from '../store/workspace';
import type { OpenTab } from '../types';

const openModels = new Map<string, monaco.editor.ITextModel>();

/** Replaces the model's full content WITHOUT `setValue`, which wipes the undo
 * stack and cursor position. A pushed edit operation keeps both intact, so an
 * external reload stays reversible with Cmd+Z. */
function replaceModelValue(model: monaco.editor.ITextModel, next: string) {
  if (model.getValue() === next) return;
  model.pushEditOperations(
    [],
    [{ range: model.getFullModelRange(), text: next }],
    () => null,
  );
}

function getModel(tab: OpenTab): monaco.editor.ITextModel {
  const uri = monaco.Uri.file(tab.path);
  let model = openModels.get(tab.path) ?? monaco.editor.getModel(uri);
  if (!model) {
    model = monaco.editor.createModel(tab.content, tab.language, uri);
    openModels.set(tab.path, model);
  } else {
    replaceModelValue(model, tab.content);
  }
  return model;
}

export const EditorPane: React.FC = () => {
  const editorHostRef = useRef<HTMLDivElement>(null);
  const editorRef = useRef<monaco.editor.IStandaloneCodeEditor | null>(null);
  const suppressChange = useRef(false);

  const tabs = useWorkspace((s) => s.tabs);
  const activePath = useWorkspace((s) => s.activePath);
  const pendingCursor = useWorkspace((s) => s.pendingCursor);
  const setActive = useWorkspace((s) => s.setActive);
  const closeTab = useWorkspace((s) => s.closeTab);
  const updateContent = useWorkspace((s) => s.updateContent);
  const saveFile = useWorkspace((s) => s.saveFile);

  const activeTab = tabs.find((t) => t.path === activePath) ?? null;

  useEffect(() => {
    monaco.editor.setTheme('kuda-dark');
    if (!editorHostRef.current || editorRef.current) return;
    const editor = monaco.editor.create(editorHostRef.current, {
      theme: 'kuda-dark',
      automaticLayout: true,
      fontFamily: "'JetBrains Mono', monospace",
      fontSize: 15,
      fontLigatures: true,
      minimap: { enabled: true, scale: 1 },
      smoothScrolling: true,
      cursorBlinking: 'smooth',
      cursorSmoothCaretAnimation: 'on',
      scrollBeyondLastLine: false,
      padding: { top: 12, bottom: 12 },
      tabSize: 4,
      renderLineHighlight: 'all',
      bracketPairColorization: { enabled: true },
    });
    editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS, () => {
      const current = useWorkspace.getState().activePath;
      if (current) saveFile(current);
    });
    editor.onDidChangeModelContent(() => {
      if (suppressChange.current) return;
      const current = useWorkspace.getState().activePath;
      const model = editor.getModel();
      if (current && model) updateContent(current, model.getValue());
    });
    editorRef.current = editor;
    return () => {
      editor.dispose();
      editorRef.current = null;
      // Dispose every tracked model: on project close `tabs` is cleared and
      // this component unmounts in the SAME commit, so the tabs-diff cleanup
      // effect never runs and all document snapshots leaked.
      for (const [, model] of openModels) model.dispose();
      openModels.clear();
    };
  }, [saveFile, updateContent]);

  useEffect(() => {
    const editor = editorRef.current;
    if (!editor) return;
    if (!activeTab) {
      editor.setModel(null);
      return;
    }
    const model = getModel(activeTab);
    suppressChange.current = true;
    editor.setModel(model);
    suppressChange.current = false;
    editor.focus();
  }, [activePath, tabs.length]);

  useEffect(() => {
    const editor = editorRef.current;
    if (!editor || !pendingCursor || pendingCursor.path !== activePath) return;
    const model = editor.getModel();
    if (!model) return;
    const line = Math.min(Math.max(1, pendingCursor.line), model.getLineCount());
    editor.revealLineInCenter(line);
    editor.setPosition({ lineNumber: line, column: 1 });
    editor.focus();
    useWorkspace.setState({ pendingCursor: null });
  }, [pendingCursor, activePath]);

  useEffect(() => {
    const current = new Set(tabs.map((t) => t.path));
    for (const [path, model] of Array.from(openModels.entries())) {
      if (!current.has(path)) {
        model.dispose();
        openModels.delete(path);
      }
    }
  }, [tabs]);

  const reloadingFromDisk = useWorkspace((s) => s.reloadingFromDisk);
  useEffect(() => {
    const editor = editorRef.current;
    if (!editor) return;
    for (const path of Object.keys(reloadingFromDisk)) {
      const tab = useWorkspace.getState().tabs.find((t) => t.path === path);
      const model = openModels.get(path);
      if (tab && model) {
        suppressChange.current = true;
        replaceModelValue(model, tab.content);
        suppressChange.current = false;
      }
    }
  }, [reloadingFromDisk]);

  return (
    <div className="editor-pane">
      <div className="tab-bar">
        {tabs.map((tab) => (
          <div
            key={tab.path}
            className={`editor-tab ${tab.path === activePath ? 'active' : ''}`}
            onClick={() => setActive(tab.path)}
            title={tab.path}
          >
            <span className="tab-name">{tab.name}</span>
            {isDirty(tab) && <Circle size={8} className="tab-dirty-dot" fill="currentColor" />}
            <button
              className="tab-close"
              onClick={(e) => {
                e.stopPropagation();
                closeTab(tab.path);
              }}
            >
              <X size={13} />
            </button>
          </div>
        ))}
      </div>
      <div className="editor-host" ref={editorHostRef} />
    </div>
  );
};
