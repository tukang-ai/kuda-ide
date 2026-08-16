import React, { useEffect, useState } from 'react';
import { Braces, FileCode2 } from 'lucide-react';
import * as ipc from '../lib/ipc';
import { useWorkspace } from '../store/workspace';
import type { CodeSymbol } from '../types';

export const OutlinePanel: React.FC = () => {
  const activePath = useWorkspace((s) => s.activePath);
  const openFile = useWorkspace((s) => s.openFile);
  const [symbols, setSymbols] = useState<CodeSymbol[] | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    let cancelled = false;
    setSymbols(null);
    if (!activePath) return;
    setLoading(true);
    ipc
      .parseSymbols(activePath)
      .then((result) => {
        if (!cancelled) setSymbols(result);
      })
      .catch(() => {
        if (!cancelled) setSymbols([]);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [activePath]);

  if (!activePath) {
    return (
      <div className="sidebar-content">
        <div className="sidebar-header"><span className="sidebar-title">Outline</span></div>
        <div className="text-muted search-empty">Open a file to see its symbols.</div>
      </div>
    );
  }

  return (
    <div className="sidebar-content">
      <div className="sidebar-header">
        <span className="sidebar-title">Outline</span>
        <span className="text-muted" style={{ fontSize: 11 }}>{activePath.split('/').pop()}</span>
      </div>
      <div className="tree-container">
        {loading && <div className="text-muted search-empty">Parsing…</div>}
        {symbols !== null && symbols.length === 0 && !loading && (
          <div className="text-muted search-empty">No symbols (unsupported language or empty file).</div>
        )}
        {symbols?.map((sym, i) => (
          <div
            key={`${sym.name}-${i}`}
            className="tree-row"
            onClick={() => openFile(sym.file_path, sym.start_line)}
            title={`${sym.name} (${sym.start_line}-${sym.end_line})`}
          >
            {sym.kind === 'Struct' || sym.kind === 'Class' ? (
              <Braces size={13} className="icon-amber" />
            ) : (
              <FileCode2 size={13} className="icon-accent" />
            )}
            <span className="tree-name symbol-name">{sym.signature ?? sym.name}</span>
            <span className="symbol-range">{sym.start_line}</span>
          </div>
        ))}
      </div>
    </div>
  );
};
