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
// Bytes that arrived BEFORE any TerminalHost subscribed to the session
// (`terminalSpawn` resolves and publishes before React commits the host).
// Without buffering, the shell's first prompt/MOTD was silently dropped.
const pending = new Map<string, string[]>();
const PENDING_CAP_CHARS = 100_000;

function publish(sessionId: string, data: string) {
  const subs = bus.get(sessionId);
  if (!subs || subs.size === 0) {
    const buf = pending.get(sessionId) ?? [];
    if (buf.join('').length < PENDING_CAP_CHARS) {
      buf.push(data);
      pending.set(sessionId, buf);
    }
    return;
  }
  subs.forEach((fn) => fn(data));
}

function subscribe(sessionId: string, fn: Listener): () => void {
  if (!bus.has(sessionId)) bus.set(sessionId, new Set());
  bus.get(sessionId)!.add(fn);
  // Replay anything captured before the first subscriber attached.
  const buffered = pending.get(sessionId);
  if (buffered) {
    pending.delete(sessionId);
    for (const chunk of buffered) fn(chunk);
  }
  return () => {
    bus.get(sessionId)?.delete(fn);
    if (bus.get(sessionId)?.size === 0) bus.delete(sessionId);
  };
}

function dropSessionBuffers(sessionId: string) {
  bus.delete(sessionId);
  pending.delete(sessionId);
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
  // Mirror of `sessions` for interval callbacks (which capture a stale
  // closure otherwise).
  const sessionsRef = useRef<PtyTab[]>([]);
  useEffect(() => {
    sessionsRef.current = sessions;
  }, [sessions]);

  /** Removes a tab's buffers and selects a NEIGHBOR when the killed tab was
   * active — setting activeId to null while other shells are alive used to
   * leave every remaining host display:none (panel looked dead). */
  const removeTab = (id: string) => {
    dropSessionBuffers(id);
    const current = sessionsRef.current;
    const idx = current.findIndex((t) => t.id === id);
    const remaining = current.filter((t) => t.id !== id);
    setSessions(remaining);
    setActiveId((cur) => {
      if (cur !== id) return cur;
      if (remaining.length === 0) return null;
      return remaining[Math.min(Math.max(idx, 0), remaining.length - 1)].id;
    });
  };

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
        const liveSet = new Set(live);
        const current = sessionsRef.current;
        const dead = current.filter((t) => !liveSet.has(t.id));
        if (dead.length === 0) return;
        dead.forEach((t) => dropSessionBuffers(t.id));
        const remaining = current.filter((t) => liveSet.has(t.id));
        setSessions(remaining);
        setActiveId((cur) => {
          if (!cur || liveSet.has(cur)) return cur;
          if (remaining.length === 0) return null;
          const deadIdx = current.findIndex((t) => t.id === cur);
          return remaining[Math.min(Math.max(deadIdx, 0), remaining.length - 1)].id;
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
    removeTab(id);
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
