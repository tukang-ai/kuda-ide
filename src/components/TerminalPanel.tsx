import React, { useCallback, useEffect, useRef, useState } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebLinksAddon } from '@xterm/addon-web-links';
import { Plus, TerminalSquare, X } from 'lucide-react';
import { Channel } from '@tauri-apps/api/core';
import '@xterm/xterm/css/xterm.css';
import * as ipc from '../lib/ipc';
import type { TerminalOutputPayload } from '../types';

type Listener = (data: string) => void;
const bus = new Map<string, Set<Listener>>();

function publish(sessionId: string, data: string) {
  bus.get(sessionId)?.forEach((fn) => fn(data));
}

function subscribe(sessionId: string, fn: Listener): () => void {
  if (!bus.has(sessionId)) bus.set(sessionId, new Set());
  bus.get(sessionId)!.add(fn);
  return () => bus.get(sessionId)?.delete(fn);
}

const TerminalHost: React.FC<{ sessionId: string; active: boolean }> = ({ sessionId, active }) => {
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);

  useEffect(() => {
    if (!containerRef.current) return;
    const term = new Terminal({
      fontSize: 15,
      fontFamily: "'JetBrains Mono', monospace",
      cursorBlink: true,
      allowProposedApi: true,
      scrollback: 5000,
      theme: {
        background: '#0f131a',
        foreground: '#e6edf3',
        cursor: '#38bdf8',
        selectionBackground: '#38bdf840',
        black: '#1c2128',
        red: '#f43f5e',
        green: '#10b981',
        yellow: '#f59e0b',
        blue: '#38bdf8',
        magenta: '#c084fc',
        cyan: '#67e8f9',
        white: '#e6edf3',
      },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.loadAddon(new WebLinksAddon());
    term.open(containerRef.current);
    termRef.current = term;
    fitRef.current = fit;

    requestAnimationFrame(() => {
      try {
        fit.fit();
        ipc.terminalResize(sessionId, term.cols, term.rows).catch(() => {});
      } catch { /* container not visible yet */ }
    });

    const unsub = subscribe(sessionId, (data) => term.write(data));
    const onData = term.onData((data) => {
      ipc.terminalWrite(sessionId, data).catch(() => {});
    });

    const observer = new ResizeObserver(() => {
      try {
        fit.fit();
        ipc.terminalResize(sessionId, term.cols, term.rows).catch(() => {});
      } catch { /* ignore */ }
    });
    observer.observe(containerRef.current);

    return () => {
      observer.disconnect();
      unsub();
      onData.dispose();
      term.dispose();
      termRef.current = null;
    };
  }, [sessionId]);

  useEffect(() => {
    if (active) {
      requestAnimationFrame(() => {
        try {
          fitRef.current?.fit();
          termRef.current?.focus();
        } catch { /* ignore */ }
      });
    }
  }, [active]);

  return (
    <div
      className="terminal-host"
      style={{ display: active ? 'block' : 'none', height: '100%', width: '100%' }}
      ref={containerRef}
      onClick={() => termRef.current?.focus()}
    />
  );
};

interface PtyTab {
  id: string;
  title: string;
}

export const TerminalPanel: React.FC = () => {
  const [sessions, setSessions] = useState<PtyTab[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const counter = useRef(0);
  const spawning = useRef(false);

  const spawn = useCallback(async () => {
    if (spawning.current) return;
    spawning.current = true;
    try {
      const channel = new Channel<TerminalOutputPayload>();
      let id = '';
      channel.onmessage = (payload) => {
        publish(payload.session_id, payload.data);
      };
      id = await ipc.terminalSpawn(channel);
      counter.current += 1;
      setSessions((s) => [...s, { id, title: `shell ${counter.current}` }]);
      setActiveId(id);
    } catch (err) {
      console.error('terminal spawn failed', err);
    } finally {
      spawning.current = false;
    }
  }, []);

  useEffect(() => {
    spawn();
    // Reap sessions whose shell exited on its own (user typed exit/Ctrl-D):
    // drop their tabs so dead terminals do not accumulate.
    const poll = setInterval(async () => {
      try {
        const live = await ipc.terminalList();
        setSessions((s) => {
          if (s.every((t) => live.includes(t.id))) return s;
          const remaining = s.filter((t) => live.includes(t.id));
          return remaining;
        });
      } catch { /* backend not ready */ }
    }, 3000);
    // The panel is unmounted when the terminal is toggled off — kill every
    // shell so toggling the panel does not leak zombie PTY processes.
    return () => {
      clearInterval(poll);
      ipc.terminalCloseAll().catch(() => {});
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const kill = async (id: string) => {
    try {
      await ipc.terminalKill(id);
    } catch { /* already gone */ }
    bus.delete(id);
    setSessions((s) => s.filter((t) => t.id !== id));
    setActiveId((current) => {
      if (current !== id) return current;
      return null;
    });
  };

  return (
    <div className="terminal-panel">
      <div className="terminal-tabs">
        <span className="terminal-panel-title">
          <TerminalSquare size={13} /> TERMINAL
        </span>
        {sessions.map((s) => (
          <div
            key={s.id}
            className={`editor-tab ${s.id === activeId ? 'active' : ''}`}
            onClick={() => setActiveId(s.id)}
          >
            <span className="tab-name">{s.title}</span>
            <button
              className="tab-close"
              onClick={(e) => {
                e.stopPropagation();
                kill(s.id);
              }}
            >
              <X size={13} />
            </button>
          </div>
        ))}
        <button className="icon-btn" title="New terminal" onClick={spawn}>
          <Plus size={14} />
        </button>
      </div>
      <div className="terminal-hosts">
        {sessions.map((s) => (
          <TerminalHost key={s.id} sessionId={s.id} active={s.id === activeId} />
        ))}
      </div>
    </div>
  );
};
