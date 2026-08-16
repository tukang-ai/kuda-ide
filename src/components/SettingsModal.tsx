import React, { useEffect, useRef, useState } from 'react';
import {
  KeyRound, Eye, EyeOff, X, Plus, Trash2, Save, Layers, Workflow, ShieldAlert, CheckCircle2,
  Sparkles, Github, Zap, Check, Activity, LogOut,
} from 'lucide-react';
import * as ipc from '../lib/ipc';
import { useLayout } from '../store/layout';
import { useAgent } from '../store/agent';

type ProviderDraft = {
  id: string | null;
  name: string;
  base_url: string;
  modelsStr: string;
  api_key: string;
};

const emptyDraft = (): ProviderDraft => ({
  id: null,
  name: '',
  base_url: '',
  modelsStr: '',
  api_key: '',
});

/// Placeholder shown in the API key field when a key is already stored, so the
/// user can see the provider is configured without revealing the secret. The
/// placeholder is never sent to the backend (treated as "no change" on save).
const KEY_MASK = '••••••••••••••';

/// fetch with a hard timeout so a half-open connection to the Hub Server can
/// never hang the UI (e.g. leave the Sign In button stuck "Waiting for browser…").
const fetchWithTimeout = async (url: string, opts: RequestInit = {}, ms = 8000): Promise<Response> => {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), ms);
  try {
    return await fetch(url, { ...opts, signal: ctrl.signal });
  } finally {
    clearTimeout(timer);
  }
};

const toModels = (drafts: ProviderDraft[]): ipc.ProviderInfo[] =>
  drafts
    .filter((d) => d.id != null)
    .map((d) => ({
      id: d.id!,
      name: d.name,
      base_url: d.base_url,
      models: d.modelsStr.split(',').map((m) => m.trim()).filter(Boolean),
      has_key: true,
    }));

/// Interactive tag / chip input for a comma-separated model list. Type a model
/// name and press Enter (or ,) to add it as a removable chip; Backspace on an
/// empty field removes the last chip. The value stays a plain comma-separated
/// string so the rest of the save flow is unchanged.
const ModelTagInput: React.FC<{ value: string; onChange: (v: string) => void }> = ({ value, onChange }) => {
  const [draft, setDraft] = useState('');
  const models = value.split(',').map((m) => m.trim()).filter(Boolean);

  const add = (raw: string) => {
    const m = raw.trim();
    if (!m) return;
    if (!models.includes(m)) {
      onChange([...models, m].join(', '));
    }
    setDraft('');
  };

  const remove = (m: string) => {
    onChange(models.filter((x) => x !== m).join(', '));
  };

  return (
    <div className="tag-input" onClick={(e) => (e.currentTarget.querySelector('input') as HTMLInputElement | null)?.focus()}>
      {models.map((m) => (
        <span key={m} className="tag-chip">
          {m}
          <button className="tag-chip-remove" onClick={(e) => { e.stopPropagation(); remove(m); }} title="Remove model">
            <X size={10} />
          </button>
        </span>
      ))}
      <input
        className="tag-input-field"
        type="text"
        placeholder={models.length === 0 ? 'e.g. deepseek-chat, deepseek-reasoner' : 'add model…'}
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ',') {
            e.preventDefault();
            add(draft);
          } else if (e.key === 'Backspace' && !draft && models.length > 0) {
            remove(models[models.length - 1]);
          }
        }}
        onBlur={() => {
          if (draft.trim()) add(draft);
        }}
      />
    </div>
  );
};

export const SettingsModal: React.FC = () => {
  const settingsOpen = useLayout((s) => s.settingsOpen);
  const setSettingsOpen = useLayout((s) => s.setSettingsOpen);
  const checkKey = useAgent((s) => s.checkKey);

  const [tab, setTab] = useState<'providers' | 'agent' | 'subscription'>('providers');
  const [selectedPlan, setSelectedPlan] = useState<'free' | 'pro' | 'developer'>('free');
  const [hubToken, setHubToken] = useState('');
  const [signingIn, setSigningIn] = useState(false);
  const [userUsage, setUserUsage] = useState<{
    plan_tier: string;
    tokens_today: number;
    tokens_remaining_today: number;
    requests_today: number;
    points_today: number;
    points_remaining_today: number;
  } | null>(null);
  const [serverPlans, setServerPlans] = useState<Array<{
    id: string;
    name: string;
    price: string;
    period: string;
    max_tokens_per_day: number;
    max_requests_per_min: number;
    features: string[];
  }>>([]);
  const [hubModels, setHubModels] = useState<ipc.HubModel[]>([]);
  const [hubRecommendations, setHubRecommendations] = useState<Record<string, string>>({});
  const [providers, setProviders] = useState<ProviderDraft[]>([]);
  const [showKeyFor, setShowKeyFor] = useState<string | null>(null);
  const [agent, setAgent] = useState<ipc.AgentConfig>({
    thinker: { provider_id: '', model: '' },
    reviewers: [{ provider_id: '', model: '' }],
    planning_writer: { provider_id: '', model: '' },
    executor_code: { provider_id: '', model: '' },
    executor_design: { provider_id: '', model: '' },
    executor_reviewer: { provider_id: '', model: '' },
    rlm_model: { provider_id: '', model: '' },
    rlm_verifier: { provider_id: '', model: '' },
  });
  const [saving, setSaving] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const [hubOnline, setHubOnline] = useState<boolean | null>(null);
  const [hubAccount, setHubAccount] = useState<ipc.HubAccount | null>(null);
  const mountedRef = useRef(true);

  useEffect(() => {
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const checkHub = async () => {
    try {
      const res = await fetchWithTimeout(`${ipc.HUB_BASE_URL}/api/v1/plans`, {}, 4000);
      setHubOnline(res.ok);
    } catch {
      setHubOnline(false);
    }
  };

  const fetchUsage = async () => {
    try {
      const headers: Record<string, string> = {};
      if (hubToken) headers['Authorization'] = `Bearer ${hubToken}`;
      const res = await fetchWithTimeout(`${ipc.HUB_BASE_URL}/api/v1/user/usage`, { headers }, 4000);
      if (res.ok) {
        const data = await res.json();
        setUserUsage(data);
        if (data.plan_tier) setSelectedPlan(data.plan_tier.toLowerCase() as any);
      }
    } catch {
      /* offline */
    }
  };

  const switchPlanTier = async (newPlan: string) => {
    try {
      const headers: Record<string, string> = { 'Content-Type': 'application/json' };
      if (hubToken) headers['Authorization'] = `Bearer ${hubToken}`;
      const res = await fetchWithTimeout(
        `${ipc.HUB_BASE_URL}/api/v1/user/plan`,
        {
          method: 'POST',
          headers,
          body: JSON.stringify({ plan_tier: newPlan }),
        },
        6000,
      );
      if (res.ok) {
        setSelectedPlan(newPlan.toLowerCase() as any);
        setNote(`Successfully subscribed to ${newPlan.toUpperCase()} plan!`);
        await fetchUsage();
      } else {
        const err = await res.json();
        setNote(`Failed to update plan: ${err.error || 'Server error'}`);
      }
    } catch (e) {
      setNote(`Server unreachable: ${e}`);
    }
  };

  const signInWithGithub = async () => {
    setSigningIn(true);
    setNote('Opening GitHub login… KudaIDE will connect automatically after you authorize.');
    const loginCode = crypto.randomUUID().replace(/-/g, '').slice(0, 16);
    try {
      try {
        const redirect = encodeURIComponent(`${ipc.HUB_BASE_URL}/api/v1/auth/github/callback`);
        const urlRes = await fetchWithTimeout(
          `${ipc.HUB_BASE_URL}/api/v1/auth/github/url?state=${loginCode}&redirect_uri=${redirect}`,
          {},
          8000,
        );
        if (!urlRes.ok) {
          const err = await urlRes.json().catch(() => ({}));
          setNote(`GitHub OAuth error: ${err.error || urlRes.status}`);
          return;
        }
        const data = await urlRes.json();
        if (!data.url) {
          setNote('GitHub OAuth is not configured on the Hub Server.');
          return;
        }
        await ipc.openExternalUrl(data.url);
      } catch {
        setNote('Hub server unreachable. Pastikan hub online di kuda-ide.my.id.');
        return;
      }

      // Poll the hub until the browser login completes (max 3 minutes). Each
      // request is time-boxed so a half-open hub can never freeze the button.
      const started = Date.now();
      while (Date.now() - started < 180000) {
        if (!mountedRef.current) return;
        await new Promise((r) => setTimeout(r, 1500));
        try {
          const res = await fetchWithTimeout(
            `${ipc.HUB_BASE_URL}/api/v1/auth/pending?code=${loginCode}`,
            { cache: 'no-store' },
            8000,
          );
          if (!res.ok) continue;
          const auth = await res.json();
          if (!auth?.token_key) continue;

          // Persist into the file-backed credential store (reliable across rebuilds);
          // the Rust side mirrors to the OS Keychain as a best-effort fallback.
          await ipc.agentSaveHubCredentials(
            auth.token_key,
            auth.session_key,
            auth.session_expires_at,
            auth.email,
            auth.plan_tier,
          );
          setHubToken(auth.token_key);
          setNote(
            `Signed in as ${auth.email} (${auth.plan_tier}) — connected automatically. Session key aktif 30 menit & auto-renew.`,
          );
          await checkKey();
          setHubAccount(await ipc.agentHubAccount().catch(() => null));
          await fetchUsage();
          return;
        } catch {
          /* hub briefly unreachable; keep polling */
        }
      }
      setNote('Login timed out. Click "Sign in with GitHub" again or paste the token manually.');
    } finally {
      setSigningIn(false);
    }
  };

  /// Loads providers and masks each stored API key (including the kuda_hub
  /// session, which lives in the file-backed credential store). An empty field
  /// means "no key configured for this provider".
  const fetchProviders = async () => {
    const provList = await ipc.providerList();
    const hasHub = await ipc.agentHasHubCredentials().catch(() => false);
    const withKeys = await Promise.all(
      provList.map(async (p) => ({
        id: p.id,
        name: p.name,
        base_url: p.base_url,
        modelsStr: p.models.join(', '),
        api_key:
          p.id === 'kuda_hub'
            ? hasHub
              ? KEY_MASK
              : ''
            : (await ipc.agentHasKey(p.id).catch(() => false))
              ? KEY_MASK
              : '',
      })),
    );
    setProviders(withKeys);
  };

  const signOut = async () => {
    try {
      await ipc.agentHubSignOut();
      setHubAccount(null);
      setHubToken('');
      setNote('Signed out of Kuda Hub.');
      await checkKey();
    } catch (err) {
      setNote(`Failed to sign out: ${err}`);
    }
  };

  const load = async () => {
    try {
      const [agentCfg] = await Promise.all([ipc.agentConfigGet()]);
      await fetchProviders();
      setAgent(agentCfg);
      try {
        setHubAccount(await ipc.agentHubAccount());
      } catch {
        setHubAccount(null);
      }

      try {
        const plansRes = await fetchWithTimeout(`${ipc.HUB_BASE_URL}/api/v1/plans`, {}, 4000);
        setHubOnline(plansRes.ok);
        if (plansRes.ok) {
          const data = await plansRes.json();
          if (data.plans) setServerPlans(data.plans);
        }
        await fetchUsage();
      } catch {
        setHubOnline(false);
        /* server offline fallback */
      }

      // Model list + rekomendasi setting default dari hub (endpoint terpisah) —
      // dipakai dropdown pemilihan model (varian sejenis bisa punya harga koin berbeda).
      try {
        const modelsRes = await fetchWithTimeout(`${ipc.HUB_BASE_URL}/api/v1/models`, {}, 4000);
        if (modelsRes.ok) {
          const data = await modelsRes.json();
          if (Array.isArray(data.data)) setHubModels(data.data);
        }
        const recRes = await fetchWithTimeout(`${ipc.HUB_BASE_URL}/api/v1/models/recommendations`, {}, 4000);
        if (recRes.ok) {
          const data = await recRes.json();
          if (data.recommendations) setHubRecommendations(data.recommendations);
        }
      } catch {
        /* hub offline — dropdown fallback ke text input */
      }
    } catch {
      /* not ready */
    }
  };

  useEffect(() => {
    if (settingsOpen) {
      load();
      setNote(null);
    }
  }, [settingsOpen]);

  // Re-check hub status whenever the subscription tab is opened, and refresh the
  // badge state so a stored credential is reflected immediately (no 30s wait).
  useEffect(() => {
    if (settingsOpen && tab === 'subscription') {
      checkHub();
      checkKey();
      ipc.agentHubAccount().then(setHubAccount).catch(() => setHubAccount(null));
    }
  }, [settingsOpen, tab]);

  // Auto-refresh the rotating Kuda Hub session key (server rotates it every 30 min;
  // refresh at most 5 min before expiry, and only while the agent is idle).
  useEffect(() => {
    if (!settingsOpen) return;
    const timer = setInterval(() => {
      if (!useAgent.getState().busy) {
        ipc.agentEnsureHubSession().catch(() => {
          /* hub offline */
        });
      }
    }, 30000);
    return () => clearInterval(timer);
  }, [settingsOpen]);

  if (!settingsOpen) return null;

  const updateProvider = (idx: number, patch: Partial<ProviderDraft>) => {
    setProviders((p) => p.map((d, i) => (i === idx ? { ...d, ...patch } : d)));
  };

  const addProvider = () => setProviders((p) => [...p, emptyDraft()]);

  const removeProvider = async (idx: number) => {
    const draft = providers[idx];
    if (draft?.id) {
      try {
        await ipc.providerDelete(draft.id);
      } catch {
        /* ignore */
      }
    }
    setProviders((p) => p.filter((_, i) => i !== idx));
  };

  const saveProviders = async () => {
    setSaving(true);
    setNote(null);
    try {
      const drafts = providers.filter(
        (d) =>
          d.id != null ||
          d.name.trim() ||
          d.base_url.trim() ||
          d.modelsStr.trim() ||
          (d.api_key.trim() && d.api_key.trim() !== KEY_MASK),
      );
      for (const d of drafts) {
        const key = d.api_key.trim() === KEY_MASK ? '' : d.api_key.trim();
        await ipc.providerSave(
          d.id,
          d.name.trim(),
          d.base_url.trim(),
          d.modelsStr.split(',').map((m) => m.trim()).filter(Boolean),
          key || null,
        );
      }
      await fetchProviders();
      setNote('Providers saved to OS Keychain & config.');
    } catch (err) {
      setNote(`Error saving providers: ${err}`);
    } finally {
      setSaving(false);
    }
  };

  const saveAgent = async () => {
    setSaving(true);
    setNote(null);
    try {
      const clean = {
        thinker: agent.thinker,
        reviewers: agent.reviewers.filter((r) => r.provider_id.trim()),
        planning_writer: agent.planning_writer,
        executor_code: agent.executor_code,
        executor_design: agent.executor_design,
        executor_reviewer: agent.executor_reviewer,
        rlm_model: agent.rlm_model,
        rlm_verifier: agent.rlm_verifier,
      };
      if (clean.reviewers.length === 0) {
        clean.reviewers = [{ provider_id: '', model: '' }];
      }
      await ipc.agentConfigSet(clean);
      setNote('Agent role assignments saved.');
      await checkKey();
    } catch (err) {
      setNote(`Error saving agent config: ${err}`);
    } finally {
      setSaving(false);
    }
  };

  const setAgentRef = (key: keyof ipc.AgentConfig, patch: Partial<ipc.ModelRef>) => {
    setAgent((a) => ({ ...a, [key]: { ...(a[key] as ipc.ModelRef), ...patch } }));
  };

  const setReviewer = (idx: number, patch: Partial<ipc.ModelRef>) => {
    setAgent((a) => ({
      ...a,
      reviewers: a.reviewers.map((r, i) => (i === idx ? { ...r, ...patch } : r)),
    }));
  };

  const addReviewer = () => {
    setAgent((a) => ({ ...a, reviewers: [...a.reviewers, { provider_id: '', model: '' }] }));
  };

  const removeReviewer = (idx: number) => {
    setAgent((a) => ({ ...a, reviewers: a.reviewers.filter((_, i) => i !== idx) }));
  };

  const providerOptions = toModels(providers);

  const ModelField: React.FC<{
    value: ipc.ModelRef;
    onChange: (patch: Partial<ipc.ModelRef>) => void;
    uid: string;
    roleKey?: string;
  }> = ({ value, onChange, uid, roleKey }) => {
    const provider = providerOptions.find((p) => p.id === value.provider_id);
    const datalistId = `dl-${uid}`;
    // Untuk provider Kuda Hub, pilihan model diambil dari list model API hub dan
    // ditampilkan sebagai dropdown (bukan text) — varian sejenis punya harga point
    // berbeda, dan yang direkomendasikan ditandai "(Recommended)".
    const isHub = value.provider_id === 'kuda_hub' && hubModels.length > 0;
    const hubOptions = isHub && roleKey ? hubModels.filter((m) => m.role === roleKey) : [];
    const hubRecommended = roleKey ? hubRecommendations[roleKey] : undefined;
    const missingOption: any[] =
      value.model && !hubOptions.some((m) => m.id === value.model)
        ? [{ id: value.model, name: value.model, input_price_per_1k: null, output_price_per_1k: null }]
        : [];    return (
      <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
        <select
          className="key-input"
          value={value.provider_id}
          onChange={(e) => onChange({ provider_id: e.target.value, model: '' })}
          style={{ flex: 1, appearance: 'auto' }}
        >
          <option value="">Select provider…</option>
          {providerOptions.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name}
            </option>
          ))}
        </select>
        {isHub && roleKey ? (
          <select
            className="key-input"
            value={value.model}
            onChange={(e) => onChange({ model: e.target.value })}
            style={{ flex: 1, appearance: 'auto' }}
          >
            <option value="">Select model…</option>
            {[...missingOption, ...hubOptions].map((m: any) => (
              <option key={m.id} value={m.id}>
                {m.name} —{' '}
                {m.input_price_per_1k != null
                  ? `${m.input_price_per_1k}/${m.output_price_per_1k} pts/1k (in/out) · cache ${m.input_price_cache_per_1k}`
                  : 'custom'}
                {m.id === hubRecommended ? ' (Recommended)' : ''}
              </option>
            ))}
          </select>
        ) : (
          <>
            <input
              className="key-input"
              list={datalistId}
              value={value.model}
              onChange={(e) => onChange({ model: e.target.value })}
              placeholder={provider?.models[0] ? provider.models[0] : 'Model…'}
              style={{ flex: 1 }}
            />
            <datalist id={datalistId}>
              {(provider?.models ?? []).map((m) => (
                <option key={m} value={m} />
              ))}
            </datalist>
          </>
        )}
      </div>
    );
  };

  const RoleRow: React.FC<{
    title: string;
    hint: string;
    value: ipc.ModelRef;
    onChange: (patch: Partial<ipc.ModelRef>) => void;
    uid: string;
    roleKey: string;
  }> = ({ title, hint, value, onChange, uid, roleKey }) => (
    <div style={{ marginBottom: 10 }}>
      <div style={{ fontSize: 12, fontWeight: 700, color: '#fff', marginBottom: 6 }}>
        {title}
        <span style={{ fontWeight: 400, color: 'var(--text-secondary)', marginLeft: 6, fontSize: 11 }}>{hint}</span>
      </div>
      <ModelField value={value} onChange={onChange} uid={uid} roleKey={roleKey} />
    </div>
  );

  const fieldStyle = {
    fontSize: 12,
    fontWeight: 600,
    color: 'var(--text-secondary)',
    marginBottom: 4,
    display: 'block' as const,
  };

  return (
    <div className="settings-modal-overlay" onClick={() => setSettingsOpen(false)}>
      <div className="settings-modal glass-panel" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-header-title">
            <KeyRound size={16} className="icon-accent" />
            <span className="gradient-text" style={{ fontWeight: 700, fontSize: 16 }}>
              LLM & Agent Settings
            </span>
          </div>
          <button className="icon-btn" onClick={() => setSettingsOpen(false)}>
            <X size={15} />
          </button>
        </div>

        <div className="segmented-tabs" role="tablist">
          <button
            className={`segmented-tab ${tab === 'providers' ? 'active' : ''}`}
            role="tab"
            aria-selected={tab === 'providers'}
            onClick={() => setTab('providers')}
          >
            <Layers size={14} /> Providers
          </button>
          <button
            className={`segmented-tab ${tab === 'agent' ? 'active' : ''}`}
            role="tab"
            aria-selected={tab === 'agent'}
            onClick={() => setTab('agent')}
          >
            <Workflow size={14} /> Agent Roles
          </button>
          <button
            className={`segmented-tab ${tab === 'subscription' ? 'active' : ''}`}
            role="tab"
            aria-selected={tab === 'subscription'}
            onClick={() => setTab('subscription')}
          >
            <Sparkles size={14} /> Subscription & Hub
          </button>
        </div>

        <div className="settings-modal-body">
          {tab === 'providers' && (
            <div className="modal-section">
              <div className="modal-section-title" style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
                <span style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
                  <Layers size={14} /> LLM Providers
                </span>
                <button className="icon-btn" title="Add provider" onClick={addProvider}>
                  <Plus size={14} />
                </button>
              </div>
              <p className="text-muted" style={{ fontSize: 12, lineHeight: 1.5, marginBottom: 12 }}>
                Each provider has its own <strong>API key</strong>, <strong>HTTP base URL</strong>, and list of <strong>model names</strong>. API keys are stored per-provider in the OS Keychain.
              </p>

              {providers.length === 0 && (
                <p className="text-muted" style={{ fontSize: 12, fontStyle: 'italic', marginBottom: 12 }}>
                  No providers yet. Click + to add your first provider (e.g. OpenAI, DeepSeek, Ollama, OpenRouter).
                </p>
              )}

              {providers.map((p, idx) => (
                <div key={idx} style={{ border: '1px solid var(--border-subtle)', borderRadius: 8, padding: 10, marginBottom: 10, position: 'relative' }}>
                  <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 8 }}>
                    <span style={{ fontSize: 12, fontWeight: 700, color: '#fff' }}>
                      {p.name || `Provider ${idx + 1}`}
                    </span>
                    {p.id && (
                      <button className="icon-btn" title="Delete provider" onClick={() => removeProvider(idx)}>
                        <Trash2 size={13} />
                      </button>
                    )}
                  </div>
                  <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
                    <div>
                      <label style={fieldStyle}>Name</label>
                      <input className="key-input" type="text" placeholder="DeepSeek" value={p.name} onChange={(e) => updateProvider(idx, { name: e.target.value })} />
                    </div>
                    <div>
                      <label style={fieldStyle}>HTTP Base URL</label>
                      <input className="key-input" type="text" placeholder="https://api.deepseek.com/v1" value={p.base_url} onChange={(e) => updateProvider(idx, { base_url: e.target.value })} />
                    </div>
                    <div>
                      <label style={fieldStyle}>Model Names</label>
                      <ModelTagInput value={p.modelsStr} onChange={(v) => updateProvider(idx, { modelsStr: v })} />
                    </div>
                    <div>
                      <label style={fieldStyle}>API Key</label>
                      <div className="key-row key-row-inline">
                        <input
                          className="key-input"
                          type={showKeyFor === p.id || p.id === null ? 'text' : 'password'}
                          placeholder="sk-…"
                          value={p.api_key}
                          onChange={(e) => updateProvider(idx, { api_key: e.target.value })}
                        />
                        <button className="icon-btn" onClick={() => setShowKeyFor(showKeyFor === p.id ? null : p.id)} title={showKeyFor === p.id ? 'Hide API key' : 'Show API key'}>
                          {showKeyFor === p.id ? <EyeOff size={14} /> : <Eye size={14} />}
                        </button>
                      </div>
                    </div>
                  </div>
                </div>
              ))}
            </div>
          )}

          {tab === 'agent' && (
            <div className="modal-section">
              <div className="modal-section-title">
                <Workflow size={14} /> Agent Role Assignment
              </div>
              <p className="text-muted" style={{ fontSize: 12, lineHeight: 1.5, marginBottom: 12 }}>
                Assign a provider + model to each agent role. The <strong>RLM Model</strong> and <strong>RLM Verifier</strong> are cheap models that collect & validate context before the <strong>Thinker</strong> writes a short temporary conclusion. The <strong>Planning Writer</strong> (cheap) then drafts the FULL detailed plan from that direction; the <strong>Thinker</strong> only reads it and approves or sends revision notes — so the costly plan-writing output stays on the cheap model. The <strong>Thinker</strong>, <strong>Reviewer</strong>, and <strong>Executor Reviewer</strong> are separate roles with their own model. You can add multiple <strong>Reviewers</strong>.
              </p>

              {providerOptions.length === 0 && (
                <p className="text-muted" style={{ fontSize: 12, fontStyle: 'italic', marginBottom: 12 }}>
                  No providers configured. Add a provider in the Providers tab first.
                </p>
              )}

              <div style={{ fontSize: 11, fontWeight: 700, color: 'var(--text-muted)', textTransform: 'uppercase', letterSpacing: 0.5, marginBottom: 6, marginTop: 4 }}>
                RLM Phase (runs before Thinker)
              </div>
              <RoleRow title="RLM Model" hint="collection + reasoning" value={agent.rlm_model} onChange={(p) => setAgentRef('rlm_model', p)} uid="rlm_model" roleKey="rlm_model" />
              <RoleRow title="RLM Verifier" hint="completeness & safety check" value={agent.rlm_verifier} onChange={(p) => setAgentRef('rlm_verifier', p)} uid="rlm_verifier" roleKey="rlm_verifier" />

              <div style={{ fontSize: 11, fontWeight: 700, color: 'var(--text-muted)', textTransform: 'uppercase', letterSpacing: 0.5, marginBottom: 6, marginTop: 12 }}>
                Planning & Execution
              </div>
              <RoleRow title="Thinker" hint="direction + plan review" value={agent.thinker} onChange={(p) => setAgentRef('thinker', p)} uid="thinker" roleKey="thinker" />
              <RoleRow title="Planning Writer" hint="drafts the full plan (cheap)" value={agent.planning_writer} onChange={(p) => setAgentRef('planning_writer', p)} uid="planning_writer" roleKey="planning_writer" />
              <RoleRow title="Executor Code" hint="code edits" value={agent.executor_code} onChange={(p) => setAgentRef('executor_code', p)} uid="exec_code" roleKey="executor_code" />
              <RoleRow title="Executor Design" hint="UI / CSS edits" value={agent.executor_design} onChange={(p) => setAgentRef('executor_design', p)} uid="exec_design" roleKey="executor_design" />
              <RoleRow title="Executor Reviewer" hint="verifies applied changes" value={agent.executor_reviewer} onChange={(p) => setAgentRef('executor_reviewer', p)} uid="exec_reviewer" roleKey="executor_reviewer" />

              <div style={{ marginTop: 4 }}>
                <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 6 }}>
                  <div style={{ fontSize: 12, fontWeight: 700, color: '#fff' }}>
                    Reviewers
                    <span style={{ fontWeight: 400, color: 'var(--text-secondary)', marginLeft: 6, fontSize: 11 }}>
                      each critiques & revises the plan in sequence
                    </span>
                  </div>
                  <button className="icon-btn" title="Add reviewer" onClick={addReviewer}>
                    <Plus size={14} />
                  </button>
                </div>
                {agent.reviewers.map((r, idx) => (
                  <div key={idx} style={{ marginBottom: 10, position: 'relative', border: '1px solid var(--border-subtle)', borderRadius: 8, padding: 10 }}>
                    <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 6 }}>
                      <span style={{ fontSize: 12, fontWeight: 700, color: '#fff' }}>Reviewer {idx + 1}</span>
                      {idx >= 1 && (
                        <button className="icon-btn" onClick={() => removeReviewer(idx)}>
                          <Trash2 size={13} />
                        </button>
                      )}
                    </div>
                    <ModelField value={r} onChange={(p) => setReviewer(idx, p)} uid={`rev-${idx}`} roleKey="reviewer" />
                  </div>
                ))}
              </div>
            </div>
          )}

          {tab === 'subscription' && (
            <div className="modal-section">
              <div className="modal-section-title" style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
                <span style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
                  <Sparkles size={14} className="icon-accent" /> Developer Subscription & Hub Server
                </span>
                <span
                  style={{
                    fontSize: 11,
                    fontWeight: 600,
                    padding: '2px 8px',
                    borderRadius: 4,
                    background:
                      hubOnline === false
                        ? 'rgba(239,68,68,0.15)'
                        : 'rgba(56,189,248,0.15)',
                    color: hubOnline === false ? '#f87171' : 'var(--accent)',
                  }}
                >
                  {hubOnline === false
                    ? 'Hub Offline'
                    : hubOnline === true
                      ? 'Hub Online'
                      : 'Checking Hub…'}
                </span>
              </div>
              <p className="text-muted" style={{ fontSize: 12, lineHeight: 1.5, marginBottom: 14 }}>
                Dynamic plan tiers & pricing are loaded directly from the Hub Server (<code>{ipc.HUB_BASE_URL}/api/v1/plans</code>) so limits stay automatically synchronized across devices.
              </p>

              {/* GitHub OAuth & Token Bar */}
              <div style={{ background: 'rgba(255,255,255,0.04)', border: '1px solid var(--border-subtle)', borderRadius: 10, padding: 12, marginBottom: 14 }}>
                <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 10 }}>
                  <span style={{ fontSize: 12, fontWeight: 700, color: '#fff' }}>Developer Authentication</span>
                  {hubAccount?.logged_in ? (
                    <span style={{ display: 'flex', alignItems: 'center', gap: 6, fontSize: 12, fontWeight: 700, color: '#34d399' }}>
                      <CheckCircle2 size={14} /> Connected
                    </span>
                  ) : (
                    <button
                      className="github-login-btn"
                      onClick={() => signInWithGithub()}
                      disabled={signingIn}
                      title="Authenticate via GitHub OAuth — auto-connects without copying the token"
                    >
                      <Github size={14} /> {signingIn ? 'Waiting for browser…' : 'Sign in with GitHub'}
                    </button>
                  )}
                </div>

                {hubAccount?.logged_in ? (
                  <div style={{ display: 'flex', flexDirection: 'column', gap: 6, fontSize: 13 }}>
                    <div style={{ color: '#e5e7eb' }}>
                      Signed in as <strong style={{ color: '#fff' }}>{hubAccount.email}</strong>
                      <span style={{ marginLeft: 8, textTransform: 'uppercase', fontSize: 11, fontWeight: 700, color: 'var(--accent)' }}>
                        {hubAccount.plan_tier} plan
                      </span>
                    </div>
                    <div className="text-muted" style={{ fontSize: 12 }}>
                      Session key aktif s/d{' '}
                      {hubAccount.session_expires_at
                        ? new Date(hubAccount.session_expires_at).toLocaleString()
                        : '(unknown)'}{' '}
                      — diperbarui otomatis sebelum habis.
                    </div>
                    <div style={{ marginTop: 6 }}>
                      <button className="primary-btn" style={{ background: 'rgba(239,68,68,0.85)' }} onClick={signOut}>
                        <LogOut size={13} /> Sign Out
                      </button>
                    </div>
                  </div>
                ) : (
                  <div>
                    <label style={fieldStyle}>Developer Token (kuda_tok_...)</label>
                    <div className="key-row">
                      <input
                        className="key-input"
                        type="password"
                        placeholder="kuda_tok_xxxxxxxxxxxxxxxx"
                        value={hubToken}
                        onChange={(e) => setHubToken(e.target.value)}
                      />
                      <button
                        className="primary-btn"
                        onClick={async () => {
                          if (!hubToken.trim()) return;
                          const master = hubToken.trim();
                          try {
                            await ipc.agentSaveKey('kuda_hub_master', master);
                            // Fetch the rotating session key directly from the hub.
                            try {
                              const r = await fetchWithTimeout(
                                `${ipc.HUB_BASE_URL}/api/v1/auth/refresh`,
                                {
                                  method: 'POST',
                                  headers: { Authorization: `Bearer ${master}` },
                                },
                                6000,
                              );
                              if (r.ok) {
                                const info = await r.json();
                                await ipc.agentSaveHubCredentials(
                                  info.token_key,
                                  info.session_key,
                                  info.session_expires_at,
                                  info.email,
                                  info.plan_tier,
                                );
                                setNote(
                                  `Hub token saved! Session key (${info.session_key.slice(0, 12)}...) aktif 30 menit dan otomatis diperbarui sebelum habis.`,
                                );
                              } else {
                                const err = await r.json().catch(() => ({}));
                                setNote(`Token tersimpan, tapi hub menolak refresh: ${err.error || r.status}`);
                              }
                            } catch {
                              // Hub offline: simpan token mentah sebagai fallback.
                              await ipc.agentSaveHubCredentials(master, master, '', '', '');
                              setNote('Hub server tidak terjangkau; token disimpan sebagai fallback.');
                            }
                            await checkKey();
                            setHubAccount(await ipc.agentHubAccount().catch(() => null));
                          } catch (err) {
                            setNote(`Failed to save token: ${err}`);
                          }
                        }}
                        style={{ padding: '0 12px', height: 32 }}
                      >
                        <Save size={13} /> Save Token
                      </button>
                    </div>
                  </div>
                )}
              </div>

              {/* Quota Progress Summary */}
              <div style={{ background: 'rgba(255,255,255,0.03)', border: '1px solid var(--border-subtle)', borderRadius: 10, padding: 12, marginBottom: 14 }}>
                <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', fontSize: 12, fontWeight: 700, color: '#fff' }}>
                  <span style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
                    <Activity size={13} style={{ color: 'var(--accent)' }} /> Today's Quota Consumption
                  </span>
                  <span>Active Plan: <strong style={{ color: 'var(--accent)', textTransform: 'uppercase' }}>{userUsage?.plan_tier || selectedPlan}</strong></span>
                </div>
                <div className="quota-progress-track">
                  <div
                    className="quota-progress-fill"
                    style={{
                      width: `${Math.min(
                        100,
                        Math.round(
                          ((userUsage?.points_today || 0) /
                            ((userUsage?.points_today || 0) + (userUsage?.points_remaining_today || 200))) *
                            100
                        )
                      )}%`,
                    }}
                  />
                </div>
                <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', fontSize: 11, color: 'var(--text-secondary)' }}>
                  <span>{(userUsage?.points_today || 0).toLocaleString()} Points Used Today</span>
                  <span>Remaining: {(userUsage?.points_remaining_today ?? 0).toLocaleString()} Points</span>
                </div>
                <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', fontSize: 11, color: 'var(--text-secondary)', marginTop: 4 }}>
                  <span>{(userUsage?.tokens_today || 0).toLocaleString()} Tokens Used Today</span>
                  <span>Each request deducts the model's point price</span>
                </div>
              </div>

              {/* Dynamic Plan Cards from Server */}
              <div style={{ fontSize: 12, fontWeight: 700, color: '#fff', marginBottom: 6 }}>
                Available Subscription Plans (Loaded from Hub Server)
              </div>
              <div className="subscription-cards-grid">
                {(serverPlans.length > 0
                  ? serverPlans
                  : [
                      {
                        id: 'free',
                        name: 'Free Developer Tier',
                        price: '$0',
                        period: '/month',
                        max_tokens_per_day: 1000000,
                        max_requests_per_min: 20,
                        features: ['1,000,000 Tokens / day (input)', '30,000 Tokens / request (output)', '20 Requests / min'],
                      },
                      {
                        id: 'pro',
                        name: 'Pro Developer Plan',
                        price: '$10',
                        period: '/month',
                        max_tokens_per_day: 1000000,
                        max_requests_per_min: 100,
                        features: ['1,000,000 Tokens / day', '100 Requests / min', 'Gemini Pro & GPT-4o Access'],
                      },
                      {
                        id: 'developer',
                        name: 'Enterprise Plan',
                        price: '$29',
                        period: '/month',
                        max_tokens_per_day: 10000000,
                        max_requests_per_min: 500,
                        features: ['10,000,000 Tokens / day', '500 Requests / min', 'Dedicated Proxy Access'],
                      },
                    ]
                ).map((plan: any) => (
                  <div
                    key={plan.id}
                    className={`plan-card ${selectedPlan === plan.id ? 'active' : ''}`}
                    onClick={() => setSelectedPlan(plan.id as any)}
                  >
                    {selectedPlan === plan.id && <span className="plan-badge-active">Active</span>}
                    <div>
                      <div className="plan-card-title">{plan.name}</div>
                      <div className="plan-card-price">
                        {plan.price} <span>{plan.period}</span>
                      </div>
                      <div className="plan-features-list">
                        {plan.features.map((feat: string, i: number) => (
                          <div key={i} className="plan-feature-item">
                            <Check size={11} style={{ color: 'var(--accent-emerald)', flexShrink: 0 }} />
                            <span>{feat}</span>
                          </div>
                        ))}
                      </div>
                    </div>

                    <button
                      className={`plan-select-btn ${selectedPlan === plan.id ? 'active-btn' : ''}`}
                      onClick={(e) => {
                        e.stopPropagation();
                        switchPlanTier(plan.id);
                      }}
                    >
                      {selectedPlan === plan.id ? (
                        <>
                          <Check size={12} /> Current Plan
                        </>
                      ) : (
                        <>
                          <Zap size={12} /> Subscribe {plan.price}
                        </>
                      )}
                    </button>
                  </div>
                ))}
              </div>
            </div>
          )}

          {note && (
            <div className="modal-note" style={{ marginTop: 10, fontSize: 12, color: '#ffffff', display: 'flex', alignItems: 'center', gap: 6 }}>
              <CheckCircle2 size={14} style={{ color: 'var(--accent-emerald)' }} /> {note}
            </div>
          )}

          <div className="modal-section">
            <div className="modal-section-title">
              <ShieldAlert size={14} /> Security & Path Isolation
            </div>
            <p className="text-muted" style={{ fontSize: 12, lineHeight: 1.6 }}>
              All agent file operations are strictly guarded by <code>PathGuard</code>. Writes trigger an automatic full-file checkpoint so you can revert any AI changes from history at any time.
            </p>
          </div>
        </div>

        <div className="modal-footer">
          <button className="icon-btn" onClick={() => setSettingsOpen(false)}>
            Cancel
          </button>
          {tab === 'providers' && (
            <button className="primary-btn" onClick={saveProviders} disabled={saving}>
              <Save size={14} /> {saving ? 'Saving…' : 'Save Providers'}
            </button>
          )}
          {tab === 'agent' && (
            <button className="primary-btn" onClick={saveAgent} disabled={saving}>
              <Save size={14} /> {saving ? 'Saving…' : 'Save Agent Config'}
            </button>
          )}
        </div>
      </div>
    </div>
  );
};