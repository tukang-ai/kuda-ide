import React, { useEffect, useMemo, useRef, useState } from 'react';
import Markdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeHighlight from 'rehype-highlight';
import {
  Bot, Brain, Check, ChevronDown, ClipboardCheck, History, KeyRound, MoreHorizontal, Network, PencilLine, Plus, RotateCcw, SearchCheck, Send, ShieldAlert, Sparkles, Square, TerminalSquare, Trash2, User, Wrench, X,
} from 'lucide-react';
import 'highlight.js/styles/github-dark.css';
import { useAgent, UiMessage, UiToolCall, PendingExternalRequest, PendingPlanDecision, PendingDirection } from '../store/agent';
import { useWorkspace, reloadOpenTabsFromDisk } from '../store/workspace';
import { useLayout } from '../store/layout';
import { HUB_BASE_URL } from '../lib/ipc';
import { buildLiveItems } from '../lib/liveItems';
import type { ChatMessage } from '../types';

const ToolCallCard = React.memo(({ call }: { call: UiToolCall }) => {
  const [showArgs, setShowArgs] = useState(false);
  const [showFull, setShowFull] = useState(false);
  const statusColor =
    call.status === 'running' ? 'var(--accent-amber)' : call.status === 'error' ? 'var(--accent-rose)' : 'var(--accent-emerald)';
  return (
    <div className="tool-call-card">
      <div className="tool-call-header" onClick={() => setShowArgs((v) => !v)}>
        <Wrench size={12} />
        <span className="tool-call-name">{call.toolName}</span>
        <span className="tool-call-status" style={{ color: statusColor }}>
          {call.status === 'running' ? 'running…' : call.status === 'error' ? 'error' : 'done'}
        </span>
      </div>
      {showArgs && (
        <pre className="tool-call-args">{prettyJson(call.argumentsJson)}</pre>
      )}
      {call.output && (
        <>
          <pre className={`tool-call-output ${call.status === 'error' ? 'error' : ''}`}>
            {showFull ? call.output : truncate(call.output, 600)}
          </pre>
          {call.output.length > 600 && (
            <button className="tool-call-toggle" onClick={() => setShowFull((v) => !v)}>
              {showFull ? 'show less' : `show ${call.output.length - 600} more`}
            </button>
          )}
        </>
      )}
    </div>
  );
});

function prettyJson(json: string): string {
  try {
    return JSON.stringify(JSON.parse(json), null, 2);
  } catch {
    return json;
  }
}

const ROLE_META: Record<string, { name: string; color: string }> = {
  thinker: { name: 'Thinker', color: '#a78bfa' },
  reviewer: { name: 'Reviewer', color: '#38bdf8' },
  planning_writer: { name: 'Planning Writer', color: '#2dd4bf' },
  plan_reviewer: { name: 'Plan Reviewer', color: '#c084fc' },
  plan_editor: { name: 'Plan Editor', color: '#10b981' },
  executor_code: { name: 'Executor Code', color: '#fbbf24' },
  executor_design: { name: 'Executor Design', color: '#f472b6' },
  executor_reviewer: { name: 'Executor Reviewer', color: '#34d399' },
  rlm_model: { name: 'RLM Model', color: '#22d3ee' },
  rlm_verifier: { name: 'RLM Verifier', color: '#f59e6b' },
  chat_coordinator: { name: 'Chat Coordinator', color: '#818cf8' },
};

/// Harga per 1k token per model dari Kuda Hub (in / cache / out).
type HubPrices = Record<string, { in: number; out: number; cache: number }>;

/// Estimasi point sebuah fase: token input non-cache × in + token cache × cache
/// + token output × out, dibagi 1000, rounded up (minimal 1). Hanya dihitung
/// untuk model yang terdaftar di hub (ada harga) dan setelah fase selesai.
const calcPhasePts = (msg: UiMessage, prices: HubPrices): number => {
  const price = msg.phaseModel ? prices[msg.phaseModel] : undefined;
  if (!price || msg.tokensIn === undefined) return 0;
  const cached = Math.min(msg.cachedIn ?? 0, msg.tokensIn);
  const uncached = msg.tokensIn - cached;
  const total = (uncached * price.in + cached * price.cache + (msg.tokensOut ?? 0) * price.out) / 1000;
  return Math.max(1, Math.ceil(total));
};

/// Angka token ringkas untuk badge sempit: 49.8k / 1.2M; angka penuh ditampilkan
/// di tooltip (title) dan saat panel lebar (via container query).
const formatTokens = (n: number): string => {
  if (!Number.isFinite(n)) return '0';
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1).replace(/\.0$/, '')}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1).replace(/\.0$/, '')}k`;
  return String(n);
};

const LiveAssistantMessage = React.memo(
  ({ msg, hubPrices }: { msg: UiMessage; hubPrices: HubPrices }) => {
    const meta = msg.agentRole ? ROLE_META[msg.agentRole] : null;
    const resumeRun = useAgent((s) => s.resumeRun);
    const busy = useAgent((s) => s.busy);
    const [collapsed, setCollapsed] = useState<boolean>(msg.minimized ?? false);
    useEffect(() => {
      setCollapsed(msg.minimized ?? false);
    }, [msg.minimized]);

    const label = msg.phaseLabel ?? meta?.name ?? 'Kuda Agent';
    const thinkingNow = !!msg.thinking;
    const writingNow = !!msg.content;
    const workingNow = msg.streaming && !thinkingNow && !writingNow && (msg.toolCalls?.length ?? 0) === 0;
    // Point dihitung dari token input/output/cache fase ini × harga per 1k token
    // model (hanya untuk provider Kuda Hub). Muncul setelah fase selesai.
    const price = msg.phaseModel ? hubPrices[msg.phaseModel] : undefined;
    const pts = calcPhasePts(msg, hubPrices);
    const cachedIn = msg.cachedIn ?? 0;
    const roleName = meta?.name ?? (label.includes(':') ? label.split(':')[0].trim() : label);
    const activity = label.includes(':') ? label.split(':').slice(1).join(':').trim() : null;
    const cacheRate = msg.tokensIn
      ? Math.round((Math.min(cachedIn, msg.tokensIn) / msg.tokensIn) * 1000) / 10
      : null;
    return (
      <div className={`agent-section ${collapsed ? 'collapsed' : ''}`}>
        <div
          className="agent-section-header"
          onClick={() => setCollapsed((v) => !v)}
          role="button"
          aria-expanded={!collapsed}
          title={collapsed ? 'Expand agent' : 'Collapse agent'}
        >
          <span
            className={`phase-status ${msg.streaming ? 'pulse' : 'done'}`}
            style={msg.streaming && meta ? { color: meta.color } : undefined}
          >
            {msg.streaming ? <span className="phase-status-dot" /> : <Check size={12} strokeWidth={3} />}
          </span>
          <span className="phase-title">
            <span className="phase-role" style={meta ? { color: meta.color } : undefined}>
              {roleName}
            </span>
            {activity && <span className="phase-activity">{activity}</span>}
            {msg.phaseModel && <span className="phase-model">{msg.phaseModel}</span>}
          </span>
          {msg.streaming && (
            <span className="phase-status-text">
              {workingNow ? 'berpikir…' : writingNow ? 'menulis…' : 'bekerja…'}
            </span>
          )}
          <span className="phase-badges">
            {cachedIn > 0 && (
              <span
                className="phase-badge badge-cache"
                title={`${cachedIn.toLocaleString()} token input cache-hit (context caching) — dihitung pakai harga cache yang lebih murah${cacheRate != null ? ` · hit rate ${cacheRate}%` : ''}`}
              >
                ⚡ {formatTokens(cachedIn)} cache
              </span>
            )}
            {pts > 0 && (
              <span
                className="phase-badge badge-pts"
                title={`Estimasi point fase ini (dipotong dari saldo Kuda Hub): ${((msg.tokensIn ?? 0) - cachedIn).toLocaleString()} token input × ${price!.in} + ${cachedIn.toLocaleString()} token cache × ${price!.cache} + ${(msg.tokensOut ?? 0).toLocaleString()} token output × ${price!.out} per 1k`}
              >
                −{pts} pts
              </span>
            )}
            {msg.tokensIn !== undefined && (
              <span
                className="phase-badge badge-tokens"
                title={`Input ${msg.tokensIn.toLocaleString()} · Output ${(msg.tokensOut ?? 0).toLocaleString()} token (perkiraan, khusus agent ini)`}
              >
                <span className="tokens-up">
                  ↑ <span className="tok-compact">{formatTokens(msg.tokensIn)}</span>
                  <span className="tok-full">{msg.tokensIn.toLocaleString()}</span>
                </span>
                <span className="tokens-sep">·</span>
                <span className="tokens-down">
                  ↓ <span className="tok-compact">{formatTokens(msg.tokensOut ?? 0)}</span>
                  <span className="tok-full">{(msg.tokensOut ?? 0).toLocaleString()}</span>
                </span>
              </span>
            )}
          </span>
          {!busy && msg.runId && (
            <button
              type="button"
              className="phase-reload-btn"
              onClick={(e) => {
                e.stopPropagation();
                resumeRun(msg.runId);
              }}
              title="Jalankan ulang / lanjutkan dari checkpoint fase ini"
            >
              <RotateCcw size={11} />
            </button>
          )}
          <span className="agent-section-chevron" aria-hidden>
            <ChevronDown size={14} />
          </span>
        </div>
        {!collapsed && (
          <div className="agent-section-body">
            {workingNow && (
              <div className="agent-typing-indicator" title="Agent sedang berpikir / menyiapkan jawaban…">
                <span /><span /><span />
              </div>
            )}
            {msg.thinking && (
              <details className="agent-thinking" open={msg.streaming || undefined}>
                <summary>
                  <Brain size={12} />
                  {msg.streaming ? 'berpikir…' : 'pemikiran'}
                </summary>
                <pre className="agent-thinking-text">{msg.thinking}</pre>
              </details>
            )}
            {msg.content &&
              (msg.streaming ? (
                <pre className="agent-streaming-text">{msg.content}</pre>
              ) : (
                <div className="markdown-body">
                  <Markdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeHighlight]}>
                    {msg.content}
                  </Markdown>
                </div>
              ))}
            {msg.toolCalls?.map((tc) => <ToolCallCard key={tc.callId} call={tc} />)}
            {msg.phaseSummary && (
              <div style={{ marginTop: 6, fontSize: 11, color: 'var(--text-muted)', fontStyle: 'italic' }}>
                ✓ {msg.phaseSummary}
              </div>
            )}
          </div>
        )}
      </div>
    );
  },
  (prev, next) =>
    prev.msg.content === next.msg.content &&
    prev.msg.thinking === next.msg.thinking &&
    prev.msg.streaming === next.msg.streaming &&
    prev.msg.toolCalls === next.msg.toolCalls &&
    prev.msg.minimized === next.msg.minimized &&
    prev.msg.phaseSummary === next.msg.phaseSummary &&
    prev.msg.tokensUsed === next.msg.tokensUsed &&
    prev.msg.tokensIn === next.msg.tokensIn &&
    prev.msg.tokensOut === next.msg.tokensOut &&
    prev.msg.cachedIn === next.msg.cachedIn &&
    prev.msg.runId === next.msg.runId &&
    prev.hubPrices === next.hubPrices,
);

const HistoryMessageView = React.memo(({ msg }: { msg: ChatMessage }) => {
  if (msg.role === 'User') {
    return (
      <div className="agent-message user">
        <div className="user-bubble">{msg.content}</div>
      </div>
    );
  }
  if (msg.role === 'Tool') {
    return (
      <div className="tool-call-card">
        <div className="tool-call-header">
          <TerminalSquare size={12} />
          <span className="tool-call-name">{msg.name ?? 'tool'}</span>
          <span className="tool-call-status" style={{ color: 'var(--text-muted)' }}>result</span>
        </div>
        <pre className="tool-call-output">{truncate(msg.content, 800)}</pre>
      </div>
    );
  }
  return (
    <div className="agent-message assistant glass-panel">
      <div className="agent-message-header">
        <Bot size={14} className="icon-accent" />
        <span>Kuda Agent</span>
      </div>
      {msg.content && (
        <div className="markdown-body">
          <Markdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeHighlight]}>
            {msg.content}
          </Markdown>
        </div>
      )}
      {msg.tool_calls?.map((tc) => (
        <div className="tool-call-card" key={tc.call_id}>
          <div className="tool-call-header">
            <Wrench size={12} />
            <span className="tool-call-name">{tc.tool_name}</span>
          </div>
        </div>
      ))}
    </div>
  );
});

function truncate(s: string, max: number): string {
  if (s.length <= max) return s;
  return `${s.slice(0, max)}…`;
}

export const PendingExternalAccessList: React.FC<{
  requests: PendingExternalRequest[];
  onAllow: (id: string) => void;
  onDeny: (id: string) => void;
}> = ({ requests, onAllow, onDeny }) => {
  if (requests.length === 0) return null;
  return (
    <div style={{ padding: '0 10px', display: 'flex', flexDirection: 'column', gap: 8 }}>
      {requests.map((r) => (
        <div
          key={r.requestId}
          className="glass-panel"
          style={{
            padding: 10,
            border: '1px solid rgba(245, 158, 11, 0.45)',
            background: 'rgba(245, 158, 11, 0.08)',
            borderRadius: 8,
            fontSize: 12,
          }}
        >
          <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 6, fontWeight: 700, color: '#fbbf24' }}>
            <ShieldAlert size={13} />
            <span>External access requested</span>
          </div>
          <div style={{ fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--text-primary)', marginBottom: 4, wordBreak: 'break-all' }}>
            {r.path}
          </div>
          <div style={{ fontSize: 11, color: 'var(--text-secondary)', marginBottom: 8, lineHeight: 1.4 }}>
            <strong>Reason:</strong> {r.reason}
            <span style={{ marginLeft: 6, color: 'var(--text-muted)' }}>({r.kind})</span>
          </div>
          <div style={{ display: 'flex', gap: 8 }}>
            <button
              className="primary-btn"
              style={{ padding: '4px 10px', fontSize: 11, justifyContent: 'center' }}
              onClick={() => onAllow(r.requestId)}
            >
              <Check size={12} /> Allow
            </button>
            <button
              className="icon-btn"
              style={{ padding: '4px 10px', fontSize: 11, border: '1px solid var(--border-subtle)' }}
              onClick={() => onDeny(r.requestId)}
            >
              <X size={12} /> Deny
            </button>
          </div>
        </div>
      ))}
    </div>
  );
};



export const PlanDecisionGateCard: React.FC<{
  pending: PendingPlanDecision;
  onDecide: (decision: 'execute' | 'revise' | 'review', note?: string) => void;
  onCancel: () => void;
}> = ({ pending, onDecide, onCancel }) => {
  const [note, setNote] = useState('');
  // Mirrors MAX_PLAN_GATE_ROUNDS in swarm.rs: after 8 rounds only execute/cancel.
  const canModify = pending.round < 8;
  const openPlanFile = useWorkspace((s) => s.openFile);
  return (
    <div
      className="glass-panel"
      style={{
        padding: 12,
        margin: '0 10px',
        border: '1px solid rgba(167, 139, 250, 0.5)',
        background: 'rgba(167, 139, 250, 0.08)',
        borderRadius: 8,
        fontSize: 12,
      }}
    >
      <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 8, fontWeight: 700, color: '#a78bfa' }}>
        <ClipboardCheck size={13} />
        <span>Plan siap — persetujuan user</span>
        <span style={{ marginLeft: 'auto', fontSize: 10, color: 'var(--text-muted)', fontWeight: 500 }}>
          round {pending.round + 1} · {pending.tasksCount} task(s)
        </span>
      </div>
      <div style={{ fontSize: 11, color: 'var(--text-secondary)', marginBottom: 8, lineHeight: 1.4 }}>
        Plan lengkap ditulis di <code style={{ fontFamily: 'var(--font-mono)' }}>{pending.planFilePath}</code>.
        Buka di editor untuk membaca detailnya, lalu pilih aksi di bawah.
      </div>

      {pending.latestNote && (
        <div
          style={{
            fontSize: 11,
            color: 'var(--text-secondary)',
            fontStyle: 'italic',
            marginBottom: 8,
            padding: '6px 8px',
            background: 'rgba(255,255,255,0.04)',
            borderRadius: 6,
          }}
        >
          Revisi terakhir: {pending.latestNote}
        </div>
      )}

      {canModify && (
        <div style={{ marginBottom: 8 }}>
          <div style={{ fontSize: 11, fontWeight: 600, color: 'var(--text-secondary)', marginBottom: 4 }}>
            Instruksi ke Thinker — apa yang salah pada plan & bagaimana seharusnya:
          </div>
          <textarea
            className="agent-prompt-textarea"
            style={{ fontSize: 12 }}
            placeholder="Contoh: task 2 kurang detail, hapus fitur X, tambahkan halaman Y, copy harus Bahasa Indonesia…"
            value={note}
            rows={3}
            onChange={(e) => setNote(e.target.value)}
          />
        </div>
      )}

      <div style={{ display: 'flex', flexWrap: 'wrap', gap: 8 }}>
        <button
          className="icon-btn"
          style={{ padding: '5px 10px', fontSize: 11, border: '1px solid var(--border-subtle)' }}
          onClick={() => openPlanFile(pending.planFilePath)}
          title={`Buka ${pending.planFilePath} di editor`}
        >
          <ClipboardCheck size={12} /> Buka plan
        </button>
        <button
          className="primary-btn"
          style={{ padding: '5px 12px', fontSize: 11, justifyContent: 'center' }}
          onClick={() => onDecide('execute')}
          title="Langsung mulai eksekusi kode — lewati Reviewer utama untuk hemat token"
        >
          <Check size={12} /> Eksekusi Langsung (Executor Code)
        </button>
        {canModify && (
          <>
            <button
              className="icon-btn"
              style={{ padding: '5px 10px', fontSize: 11, border: '1px solid var(--border-subtle)' }}
              disabled={!note.trim()}
              onClick={() => onDecide('revise', note.trim())}
              title="Kirim instruksi ini ke Thinker/Planning Writer untuk memperbaiki plan"
            >
              <PencilLine size={12} /> Minta revisi plan
            </button>
            <button
              className="icon-btn"
              style={{ padding: '5px 10px', fontSize: 11, border: '1px solid var(--border-subtle)' }}
              onClick={() => onDecide('review')}
              title="Jalankan Reviewer utama (model pintar) untuk mengaudit & memperdalam plan"
            >
              <SearchCheck size={12} /> Audit Reviewer Utama
            </button>
          </>
        )}
        <button
          className="icon-btn danger"
          style={{ padding: '5px 10px', fontSize: 11 }}
          onClick={onCancel}
          title="Batalkan run (plan draft disimpan)"
        >
          <X size={12} /> Batal
        </button>
      </div>
    </div>
  );
};

export const DirectionReviewCard: React.FC<{
  pending: PendingDirection;
  onDecide: (decision: 'lanjut' | 'ubah', note?: string) => void;
  onCancel: () => void;
}> = ({ pending, onDecide, onCancel }) => {
  const [note, setNote] = useState('');
  return (
    <div
      className="glass-panel"
      style={{
        padding: 12,
        margin: '0 10px',
        border: '1px solid rgba(56, 189, 248, 0.5)',
        background: 'rgba(56, 189, 248, 0.08)',
        borderRadius: 8,
        fontSize: 12,
      }}
    >
      <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 8, fontWeight: 700, color: '#38bdf8' }}>
        <Sparkles size={13} />
        <span>Kesimpulan sementara — review arah sebelum full plan</span>
      </div>
      <div
        style={{
          fontSize: 12,
          color: 'var(--text-secondary)',
          lineHeight: 1.5,
          marginBottom: 8,
          padding: '6px 8px',
          background: 'rgba(255,255,255,0.04)',
          borderRadius: 6,
          whiteSpace: 'pre-wrap',
        }}
      >
        {pending.conclusion}
      </div>
      <div style={{ marginBottom: 8 }}>
        <div style={{ fontSize: 11, fontWeight: 600, color: 'var(--text-secondary)', marginBottom: 4 }}>
          Ubah arah (opsional) — catatan untuk Thinker sebelum membuat full plan:
        </div>
        <textarea
          className="agent-prompt-textarea"
          style={{ fontSize: 12 }}
          placeholder="Contoh: jangan ubah file auth, fokus ke halaman utama dulu, pakai Bahasa Indonesia…"
          value={note}
          rows={2}
          onChange={(e) => setNote(e.target.value)}
        />
      </div>
      <div style={{ display: 'flex', flexWrap: 'wrap', gap: 8 }}>
        <button
          className="primary-btn"
          style={{ padding: '5px 12px', fontSize: 11, justifyContent: 'center' }}
          onClick={() => onDecide('lanjut')}
          title="Arah sudah benar — lanjut membuat full plan"
        >
          <Check size={12} /> Lanjut
        </button>
        <button
          className="icon-btn"
          style={{ padding: '5px 10px', fontSize: 11, border: '1px solid var(--border-subtle)' }}
          disabled={!note.trim()}
          onClick={() => onDecide('ubah', note.trim())}
          title="Kirim catatan arah ini ke Thinker, lalu buat full plan"
        >
          <PencilLine size={12} /> Ubah arah
        </button>
        <button
          className="icon-btn danger"
          style={{ padding: '5px 10px', fontSize: 11 }}
          onClick={onCancel}
          title="Batalkan run"
        >
          <X size={12} /> Batal
        </button>
      </div>
    </div>
  );
};

export const AgentPanel: React.FC = () => {
  const sessions = useAgent((s) => s.sessions);
  const activeSessionId = useAgent((s) => s.activeSessionId);
  const historyMessages = useAgent((s) => s.historyMessages);
  const liveMessages = useAgent((s) => s.liveMessages);
  const busy = useAgent((s) => s.busy);
  const error = useAgent((s) => s.error);
  const resumeTarget = useAgent((s) => s.resumeTarget);
  const autoApprove = useAgent((s) => s.autoApprove);
  const planGateEnabled = useAgent((s) => s.planGateEnabled);
  const pendingPlanDecision = useAgent((s) => s.pendingPlanDecision);
  const pendingDirection = useAgent((s) => s.pendingDirection);
  const hasGeminiKey = useAgent((s) => s.hasGeminiKey);
  const showHistory = useAgent((s) => s.showHistory);
  const agentMode = useAgent((s) => s.agentMode);
  const init = useAgent((s) => s.init);
  const setAutoApprove = useAgent((s) => s.setAutoApprove);
  const setPlanGateEnabled = useAgent((s) => s.setPlanGateEnabled);
  const setAgentMode = useAgent((s) => s.setAgentMode);
  const newChat = useAgent((s) => s.newChat);
  const loadHistory = useAgent((s) => s.loadHistory);
  const deleteSession = useAgent((s) => s.deleteSession);
  const toggleHistoryPanel = useAgent((s) => s.toggleHistoryPanel);
  const send = useAgent((s) => s.send);
  const resumeRun = useAgent((s) => s.resumeRun);
  const cancelRun = useAgent((s) => s.cancelRun);
  const resolvePlanDecision = useAgent((s) => s.resolvePlanDecision);
  const resolveDirection = useAgent((s) => s.resolveDirection);
  const revertEditSession = useAgent((s) => s.revertEditSession);
  const projectRoot = useWorkspace((s) => s.projectRoot);
  const toggleAgent = useLayout((s) => s.toggleAgent);
  const setSettingsOpen = useLayout((s) => s.setSettingsOpen);
  const [input, setInput] = useState('');
  const [menuOpen, setMenuOpen] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);

  // Harga per 1k token per model dari Kuda Hub (GET /api/v1/models) — dipakai
  // menghitung "≈ −N pts" tiap fase (input/output/cache) dan total point per
  // run/sesi. Kosong bila hub offline / provider non-hub.
  const [hubPrices, setHubPrices] = useState<HubPrices>({});
  useEffect(() => {
    let cancelled = false;
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), 4000);
    fetch(`${HUB_BASE_URL}/api/v1/models`, { signal: ctrl.signal })
      .then((r) => (r.ok ? r.json() : null))
      .then((data) => {
        clearTimeout(timer);
        if (cancelled || !data || !Array.isArray(data.data)) return;
        const map: HubPrices = {};
        for (const m of data.data) {
          map[m.id] = {
            in: m.input_price_per_1k ?? 0,
            cache: m.input_price_cache_per_1k ?? 0,
            out: m.output_price_per_1k ?? 0,
          };
        }
        setHubPrices(map);
      })
      .catch(() => clearTimeout(timer));
    return () => {
      cancelled = true;
      clearTimeout(timer);
      ctrl.abort();
    };
  }, []);

  useEffect(() => {
    init();
  }, [init, projectRoot]);

  const liveItems = useMemo(() => buildLiveItems(liveMessages), [liveMessages]);

  const handleScroll = () => {
    const el = scrollRef.current;
    if (el) stickToBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 60;
  };

  useEffect(() => {
    if (!stickToBottom.current) return;
    const el = scrollRef.current;
    if (!el) return;
    const id = requestAnimationFrame(() => {
      el.scrollTop = el.scrollHeight;
    });
    return () => cancelAnimationFrame(id);
  }, [liveMessages, historyMessages]);

  const submit = async () => {
    const prompt = input.trim();
    if (!prompt || busy) return;
    setInput('');
    await send(prompt, () => {
      reloadOpenTabsFromDisk();
    });
  };

  return (
    <div className="agent-panel">
      <div className="agent-header">
        <div className="agent-header-title">
          <Sparkles size={18} className="icon-accent" />
          <span className="gradient-text" style={{ fontWeight: 700, fontSize: 16 }}>Kuda Agent</span>
        </div>
        <div className="agent-header-tools">
          <button className="icon-btn" title="New chat" onClick={newChat}>
            <Plus size={18} />
          </button>
          <div style={{ position: 'relative' }}>
            <button
              className={`icon-btn ${menuOpen ? 'toggled' : ''}`}
              title="More options"
              onClick={() => setMenuOpen((v) => !v)}
            >
              <MoreHorizontal size={18} />
            </button>
            {menuOpen && (
              <>
                <div
                  style={{ position: 'fixed', inset: 0, zIndex: 30 }}
                  onClick={() => setMenuOpen(false)}
                />
                <div className="agent-header-dropdown">
                  <div style={{ fontSize: 10, fontWeight: 700, color: 'var(--text-muted)', textTransform: 'uppercase', padding: '4px 8px', letterSpacing: 0.5 }}>
                    Agent Execution Mode
                  </div>
                  <button
                    className={`dropdown-item ${agentMode === 'swarm' ? 'active' : ''}`}
                    disabled={busy}
                    onClick={() => {
                      setAgentMode('swarm');
                      setMenuOpen(false);
                    }}
                    title="Swarm Mode: Full pipeline otomatis (Thinker → Direction → Planning Writer ⇄ Reviewer → Gate → Executor → Reviewer)"
                  >
                    <Network size={13} /> Swarm Mode
                    <span className="menu-badge">{agentMode === 'swarm' ? 'ACTIVE' : ''}</span>
                  </button>
                  <button
                    className={`dropdown-item ${agentMode === 'coordinator' ? 'active' : ''}`}
                    disabled={busy}
                    onClick={() => {
                      setAgentMode('coordinator');
                      setMenuOpen(false);
                    }}
                    title="Coordinator Mode: Frontline chat cerdas yang memanggil sub-agent (RLM, Thinker, Planning, Executor) sesuai kebutuhan"
                  >
                    <Sparkles size={13} /> Coordinator Mode
                    <span className="menu-badge">{agentMode === 'coordinator' ? 'ACTIVE' : ''}</span>
                  </button>
                  <button
                    className={`dropdown-item ${agentMode === 'chat' ? 'active' : ''}`}
                    disabled={busy}
                    onClick={() => {
                      setAgentMode('chat');
                      setMenuOpen(false);
                    }}
                    title="Direct Chat: Percakapan langsung single agent cepat"
                  >
                    <Bot size={13} /> Direct Chat
                    <span className="menu-badge">{agentMode === 'chat' ? 'ACTIVE' : ''}</span>
                  </button>
                  <div style={{ height: 1, background: 'var(--border-subtle)', margin: '4px 0' }} />
                  <button
                    className={`dropdown-item ${showHistory ? 'active' : ''}`}
                    onClick={() => {
                      toggleHistoryPanel();
                      setMenuOpen(false);
                    }}
                  >
                    <History size={13} /> Chat history
                  </button>
                </div>
              </>
            )}
          </div>
          <button className="icon-btn danger" title="Close AI Agent panel (Cmd+I)" onClick={toggleAgent}>
            <X size={18} />
          </button>
        </div>
      </div>

      <div className="agent-autoapprove" title="When off, tools that modify files require manual approval and will be skipped">
        <label className="switch-label" style={{ fontSize: 13, fontWeight: 500, color: 'var(--text-secondary)' }}>
          <span className="switch">
            <input type="checkbox" checked={autoApprove} onChange={(e) => setAutoApprove(e.target.checked)} />
            <span className="switch-track"><span className="switch-thumb" /></span>
          </span>
          Auto-approve file edits
        </label>
        <label
          className="switch-label"
          style={{ fontSize: 13, fontWeight: 500, color: 'var(--text-secondary)' }}
          title="Tunggu persetujuan user setelah Thinker membuat plan: Edit / Minta Reviewer / Eksekusi. Eksekusi kode TIDAK dimulai sebelum user mengklik Eksekusi."
        >
          <span className="switch">
            <input
              type="checkbox"
              checked={planGateEnabled}
              disabled={busy}
              onChange={(e) => setPlanGateEnabled(e.target.checked)}
            />
            <span className="switch-track"><span className="switch-thumb" /></span>
          </span>
          Konfirmasi plan
        </label>
        {busy && (
          <button
            className="icon-btn danger"
            title="Stop the current agent run"
            onClick={cancelRun}
          >
            <Square size={14} />
          </button>
        )}
      </div>

      <div className="agent-stream" ref={scrollRef} onScroll={handleScroll}>
        {showHistory && (
          <div className="history-panel glass-panel">
            <div className="history-title">Chat History</div>
            {sessions.length === 0 && <div className="text-muted history-empty">No sessions yet.</div>}
            {sessions.map((s) => (
              <div key={s.session_id} className={`history-item ${s.session_id === activeSessionId ? 'active' : ''}`}>
                <div className="history-item-main" onClick={() => loadHistory(s.session_id)}>
                  <div className="history-item-title">{s.title}</div>
                  <div className="history-item-meta">
                    {new Date(s.updated_at).toLocaleString()} · {s.message_count} msgs
                  </div>
                </div>
                <button className="icon-btn danger" onClick={() => deleteSession(s.session_id)} title="Delete">
                  <Trash2 size={13} />
                </button>
              </div>
            ))}
          </div>
        )}

        {historyMessages.map((m, i) => <HistoryMessageView key={`h${i}`} msg={m} />)}
        {liveItems.map((item) =>
          item.type === 'user' ? (
            <div key={item.msg.id} className="agent-message user">
              <div className="user-bubble">{item.msg.content}</div>
              {!busy && item.msg.editSessionId && (
                <button
                  className="revert-prompt-btn"
                  title="Revert all files edited by this prompt (full-file snapshots)"
                  onClick={() => {
                    if (confirm('Revert every file edited by this prompt to its state before the prompt? Created files will be removed and deleted files restored.')) {
                      revertEditSession(item.msg.editSessionId ?? undefined, () => reloadOpenTabsFromDisk());
                    }
                  }}
                >
                  <RotateCcw size={11} /> Revert
                </button>
              )}
            </div>
          ) : (
            <div key={item.runId} className="run-box">
              {item.messages.map((m) => <LiveAssistantMessage key={m.id} msg={m} hubPrices={hubPrices} />)}
              {item.messages.some((m) => calcPhasePts(m, hubPrices) > 0) && (
                <div
                  style={{
                    marginTop: 4,
                    fontSize: 10,
                    color: 'var(--text-muted)',
                    textAlign: 'right',
                    fontWeight: 600,
                  }}
                  title="Total estimasi point semua fase di prompt ini (dipotong dari saldo Kuda Hub)"
                >
                  Total prompt ini ≈ {item.messages.reduce((s, m) => s + calcPhasePts(m, hubPrices), 0).toLocaleString()} pts
                </div>
              )}
            </div>
          ),
        )}

        {pendingDirection && (
          <DirectionReviewCard
            pending={pendingDirection}
            onDecide={resolveDirection}
            onCancel={cancelRun}
          />
        )}

        {pendingPlanDecision && (
          <PlanDecisionGateCard
            pending={pendingPlanDecision}
            onDecide={resolvePlanDecision}
            onCancel={cancelRun}
          />
        )}

        {error && (
          <div className="agent-error glass-panel">
            <span style={{ fontWeight: 600 }}>Error</span>
            <span>{error}</span>
            {resumeTarget && !busy && (
              <button
                className="primary-btn"
                style={{ padding: '5px 12px', fontSize: 11, justifyContent: 'center', marginTop: 8, alignSelf: 'flex-start' }}
                onClick={() => resumeRun()}
                title="Lanjutkan dari titik terakhir yang berhasil (riset + arah tetap, tanpa mengulang dari awal)"
              >
                <RotateCcw size={12} /> Jalankan ulang bagian akhir
              </button>
            )}
          </div>
        )}

        {historyMessages.length === 0 && liveMessages.length === 0 && hasGeminiKey && !showHistory && (
          <div className="agent-empty">
            <div className="agent-empty-icon"><Bot size={32} /></div>
            <div className="agent-empty-title">Agentic Coding Assistant</div>
            <p className="agent-empty-desc text-muted">
              I can read files, search code, and apply surgical multi-chunk edits with automatic
              full-file checkpoints. Tools: batch_file_read, multi_replace_file, list_dir,
              grep_search, code_outline, rlm_python.
            </p>
            <div className="agent-suggestions">
              {[
                'List the project structure and summarize this codebase',
                'Find all TODO comments in this project',
                'Read src/App.tsx and explain what it does',
              ].map((sug) => (
                <button key={sug} className="suggestion-chip" onClick={() => setInput(sug)}>
                  <User size={11} /> {sug}
                </button>
              ))}
            </div>
          </div>
        )}
      </div>

      <div className="agent-input-area">
        {liveItems.reduce((s, it) => s + (it.type === 'run' ? it.messages.reduce((a, m) => a + calcPhasePts(m, hubPrices), 0) : 0), 0) > 0 && (
          <div
            style={{
              marginBottom: 8,
              fontSize: 11,
              color: 'var(--text-muted)',
              textAlign: 'right',
              fontWeight: 600,
            }}
            title="Total estimasi point semua prompt di sesi chat ini"
          >
            Total sesi ini ≈{' '}
            {liveItems
              .reduce(
                (s, it) => s + (it.type === 'run' ? it.messages.reduce((a, m) => a + calcPhasePts(m, hubPrices), 0) : 0),
                0
              )
              .toLocaleString()}{' '}
            pts
          </div>
        )}
        {!hasGeminiKey && (
          <div
            className="agent-key-warning"
            onClick={() => setSettingsOpen(true)}
            style={{
              cursor: 'pointer',
              padding: '8px 14px',
              background: 'rgba(239, 68, 68, 0.18)',
              border: '1px solid rgba(239, 68, 68, 0.45)',
              borderRadius: 8,
              marginBottom: 10,
              fontSize: 13,
              display: 'flex',
              alignItems: 'center',
              gap: 8,
              color: '#fca5a5',
              fontWeight: 600,
            }}
          >
            <KeyRound size={16} /> No API Key configured. Click here to configure OpenAI / Gemini key.
          </div>
        )}
        <div className="agent-prompt-card">
          <textarea
            className="agent-prompt-textarea"
            placeholder={busy ? 'Agent is working…' : 'Ask anything, @ to mention, / for actions'}
            value={input}
            rows={2}
            disabled={busy}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => {
              // `isComposing` guard: pressing Enter to CONFIRM a CJK IME
              // candidate must not submit the half-composed prompt.
              if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
                e.preventDefault();
                submit();
              }
            }}
          />
          <div className="agent-prompt-toolbar">
            <div className="agent-prompt-tools-left">
              <button
                className="icon-btn-subtle"
                title="Attach active file as context"
                onClick={() => {
                  const tab = useWorkspace
                    .getState()
                    .tabs.find((t) => t.path === useWorkspace.getState().activePath);
                  if (!tab) return;
                  const rel =
                    projectRoot && tab.path.startsWith(projectRoot)
                      ? tab.path.slice(projectRoot.length + 1)
                      : tab.path;
                  setInput((cur) =>
                    cur.trim()
                      ? `${cur}\n\n@context file: ${rel}\n\`\`\`\n${tab.content}\n\`\`\``
                      : `@context file: ${rel}\n\`\`\`\n${tab.content}\n\`\`\``,
                  );
                }}
              >
                <Plus size={16} />
              </button>
            </div>
            <button className="send-pill-btn" onClick={submit} disabled={busy || !input.trim()} title="Send prompt">
              <Send size={16} />
            </button>
          </div>
        </div>
      </div>
    </div>
  );
};
