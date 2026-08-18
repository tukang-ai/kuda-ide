import { invoke, Channel } from '@tauri-apps/api/core';
import { open as dialogOpen } from '@tauri-apps/plugin-dialog';
import type {
  AgentRunResult,
  ChatSessionData,
  ChatSessionMeta,
  CodeSymbol,
  DiffResult,
  DirEntryItem,
  FileCheckpoint,
  FileContentPayload,
  ProjectInfo,
  SearchMatch,
  SearchQuery,
  SessionInfo,
  TerminalOutputPayload,
  WriteFileResponse,
} from '../types';

export type { Channel };

// ── Project ──────────────────────────────────────────────────────────────
export const openFolderDialog = (): Promise<string | string[] | null> =>
  dialogOpen({ directory: true, multiple: false, title: 'Open Project Folder' });

export const projectOpen = (path: string): Promise<ProjectInfo> =>
  invoke<ProjectInfo>('project_open', { path });

export const projectCurrent = (): Promise<string | null> =>
  invoke<string | null>('project_current');

export const openExternalUrl = (url: string): Promise<void> =>
  invoke<void>('open_external_url', { url });

// ── FileSystem ───────────────────────────────────────────────────────────
export const fsListDir = (path: string): Promise<DirEntryItem[]> =>
  invoke<DirEntryItem[]>('fs_list_dir', { path });

export const fsReadFile = (
  path: string,
  startLine?: number,
  endLine?: number,
): Promise<FileContentPayload> =>
  invoke<FileContentPayload>('fs_read_file', { path, startLine, endLine });

export const fsWriteFile = (
  path: string,
  content: string,
  agentMessageId?: string,
): Promise<WriteFileResponse> =>
  invoke<WriteFileResponse>('fs_write_file', { path, content, agentMessageId });

export const fsDelete = (path: string): Promise<void> =>
  invoke<void>('fs_delete', { path });

export const fsCreateDir = (path: string): Promise<void> =>
  invoke<void>('fs_create_dir', { path });

export const fsRename = (from: string, to: string): Promise<string> =>
  invoke<string>('fs_rename', { from, to });

export type FsEvent =
  | { Created: string[] }
  | { Modified: string[] }
  | { Deleted: string[] };

export const fsWatchStart = (onEvent: Channel<FsEvent>): Promise<void> =>
  invoke<void>('fs_watch_start', { onEvent });

// ── Terminal PTY ─────────────────────────────────────────────────────────
export const terminalSpawn = (
  onOutput: Channel<TerminalOutputPayload>,
  cwd?: string,
  cols?: number,
  rows?: number,
): Promise<string> =>
  invoke<string>('terminal_spawn', { onOutput, cwd, cols, rows });

export const terminalWrite = (sessionId: string, data: string): Promise<void> =>
  invoke<void>('terminal_write', { sessionId, data });

export const terminalResize = (sessionId: string, cols: number, rows: number): Promise<void> =>
  invoke<void>('terminal_resize', { sessionId, cols, rows });

export const terminalKill = (sessionId: string): Promise<void> =>
  invoke<void>('terminal_kill', { sessionId });

export const terminalList = (): Promise<string[]> =>
  invoke<string[]>('terminal_list');

export const terminalCloseAll = (): Promise<void> =>
  invoke<void>('terminal_close_all');

// ── Indexer ──────────────────────────────────────────────────────────────
export const searchCode = (query: SearchQuery): Promise<SearchMatch[]> =>
  invoke<SearchMatch[]>('search_code', { query });

export interface SearchReplaceResult {
  files_changed: number;
  matches_replaced: number;
  files: string[];
}

export const searchReplace = (
  query: SearchQuery,
  replacement: string,
): Promise<SearchReplaceResult> =>
  invoke<SearchReplaceResult>('search_replace', { query, replacement });

export const parseSymbols = (path: string): Promise<CodeSymbol[]> =>
  invoke<CodeSymbol[]>('parse_symbols', { path });

// ── Agent ────────────────────────────────────────────────────────────────
export type AgentEventChannel = Channel<import('../types').AgentEvent>;

export const agentChat = (
  userPrompt: string,
  sessionId: string | null,
  autoApprove: boolean,
  onEvent: AgentEventChannel,
  runId: string,
): Promise<AgentRunResult> =>
  invoke<AgentRunResult>('agent_chat', { userPrompt, sessionId, autoApprove, onEvent, runId });

export const agentSwarmChat = (
  userPrompt: string,
  sessionId: string | null,
  autoApprove: boolean,
  onEvent: AgentEventChannel,
  runId: string,
): Promise<AgentRunResult> =>
  invoke<AgentRunResult>('agent_swarm_chat', { userPrompt, sessionId, autoApprove, onEvent, runId });

export const agentResumeRun = (
  sessionId: string,
  runId: string,
  autoApprove: boolean,
  onEvent: AgentEventChannel,
): Promise<AgentRunResult> =>
  invoke<AgentRunResult>('agent_resume_run', { sessionId, runId, autoApprove, onEvent });

export const agentCancelRun = (runId: string): Promise<void> =>
  invoke<void>('agent_cancel_run', { runId });

export const agentApproveExternalAccess = (requestId: string): Promise<void> =>
  invoke<void>('agent_approve_external_access', { requestId });

export const agentDenyExternalAccess = (requestId: string): Promise<void> =>
  invoke<void>('agent_deny_external_access', { requestId });

export const agentResolvePlanDecision = (
  requestId: string,
  decision: 'execute' | 'revise' | 'review',
  note?: string,
): Promise<void> =>
  invoke<void>('agent_resolve_plan_decision', { requestId, decision, note });

export const agentResolveDirectionDecision = (
  requestId: string,
  decision: 'lanjut' | 'ubah',
  note?: string,
): Promise<void> =>
  invoke<void>('agent_resolve_direction_decision', { requestId, decision, note });

export const bindExternalAccessEvents = (onEvent: AgentEventChannel): Promise<void> =>
  invoke<void>('agent_bind_external_events', { onEvent });

export const agentGetConfig = (key: string): Promise<string | null> =>
  invoke<string | null>('agent_get_config', { key });

export const agentDeleteConfig = (key: string): Promise<void> =>
  invoke<void>('agent_delete_config', { key });

export const agentSaveKey = (provider: string, apiKey: string): Promise<void> =>
  invoke<void>('agent_save_key', { provider, apiKey });

export const agentHasKey = (provider: string): Promise<boolean> =>
  invoke<boolean>('agent_has_key', { provider });

/// Base URL server Kuda Hub (publik lewat Cloudflare Tunnel).
/// Semua daftar harga / plans / model diambil LANGSUNG dari sini, jadi ubah
/// sekali di hub -> otomatis sinkron di semua device tanpa edit IDE.
export const HUB_BASE_URL = 'https://kuda-ide.my.id';

export interface HubSessionInfo {
  token_key: string;
  session_key: string;
  session_expires_at: string;
  email: string;
  plan_tier: string;
}

export const agentRefreshHubSession = (): Promise<HubSessionInfo> =>
  invoke<HubSessionInfo>('agent_refresh_hub_session');

export const agentEnsureHubSession = (): Promise<void> =>
  invoke<void>('agent_ensure_hub_session');

export const agentSaveHubCredentials = (
  masterToken: string,
  sessionKey: string,
  sessionExpiresAt: string,
  email: string,
  planTier: string,
): Promise<void> =>
  invoke<void>('agent_save_hub_credentials', {
    masterToken,
    sessionKey,
    sessionExpiresAt,
    email,
    planTier,
  });

export const agentHasHubCredentials = (): Promise<boolean> =>
  invoke<boolean>('agent_has_hub_credentials');

export interface HubAccount {
  logged_in: boolean;
  email: string;
  plan_tier: string;
  session_expires_at: string;
}

export const agentHubAccount = (): Promise<HubAccount> =>
  invoke<HubAccount>('agent_hub_account');

export const agentHubUserUsage = (): Promise<any> =>
  invoke<any>('agent_hub_user_usage');

export const agentHubSignOut = (): Promise<void> =>
  invoke<void>('agent_hub_sign_out');

export const agentPollHubLogin = (verifier: string): Promise<HubAccount> =>
  invoke<HubAccount>('agent_poll_hub_login', { verifier });

export const authStartGithubPkce = (): Promise<void> =>
  invoke<void>('auth_start_github_pkce');

export const authStartLoopback = (): Promise<number> =>
  invoke<number>('auth_start_loopback');

export const authStopLoopback = (): Promise<void> =>
  invoke<void>('auth_stop_loopback');

export const authGetPickup = (): Promise<string | null> =>
  invoke<string | null>('auth_get_pickup');

export interface ProviderInfo {
  id: string;
  name: string;
  base_url: string;
  models: string[];
  has_key: boolean;
}

/// Model terdaftar di Kuda Hub (dari GET /api/v1/models). Dipakai untuk dropdown
/// pemilihan model: satu role bisa punya beberapa varian sejenis dengan harga
/// berbeda; `recommended` menandai setting default yang direkomendasikan hub.
/// Deduksi point mengikuti token input/output aktual × harga per 1k token.
export interface HubModel {
  id: string;
  role: string;
  name: string;
  provider: string;
  max_tokens: number;
  supports_tools: boolean;
  description: string;
  input_price_per_1k: number;
  input_price_cache_per_1k: number;
  output_price_per_1k: number;
  recommended: boolean;
}

export interface ModelRef {
  provider_id: string;
  model: string;
}

export interface AgentConfig {
  thinker: ModelRef;
  reviewers: ModelRef[];
  planning_writer: ModelRef;
  executor_code: ModelRef;
  executor_design: ModelRef;
  executor_reviewer: ModelRef;
  rlm_model: ModelRef;
  rlm_verifier: ModelRef;
  plan_gate_enabled?: boolean;
}

export const providerList = (): Promise<ProviderInfo[]> =>
  invoke<ProviderInfo[]>('provider_list');

export const providerSave = (
  id: string | null,
  name: string,
  baseUrl: string,
  models: string[],
  apiKey: string | null,
): Promise<ProviderInfo> =>
  invoke<ProviderInfo>('provider_save', { id, name, baseUrl, models, apiKey });

export const providerDelete = (id: string): Promise<void> =>
  invoke<void>('provider_delete', { id });

export const agentConfigGet = (): Promise<AgentConfig> =>
  invoke<AgentConfig>('agent_config_get');

export const agentConfigSet = (config: AgentConfig): Promise<void> =>
  invoke<void>('agent_config_set', { config });

export const chatListSessions = (): Promise<ChatSessionMeta[]> =>
  invoke<ChatSessionMeta[]>('chat_list_sessions');

export const chatLoadSession = (sessionId: string): Promise<ChatSessionData> =>
  invoke<ChatSessionData>('chat_load_session', { sessionId });

export const chatDeleteSession = (sessionId: string): Promise<void> =>
  invoke<void>('chat_delete_session', { sessionId });

// ── History & Diff ───────────────────────────────────────────────────────
export const historyListCheckpoints = (): Promise<FileCheckpoint[]> =>
  invoke<FileCheckpoint[]>('history_list_checkpoints');

export const historyListSessions = (): Promise<SessionInfo[]> =>
  invoke<SessionInfo[]>('history_list_sessions');

export const historyRevertSession = (sessionId: string): Promise<string[]> =>
  invoke<string[]>('history_revert_session', { sessionId });

export const historyRestoreCheckpoint = (checkpointId: string): Promise<string> =>
  invoke<string>('history_restore_checkpoint', { checkpointId });

export const diffCompute = (path: string, modifiedContent: string): Promise<DiffResult> =>
  invoke<DiffResult>('diff_compute', { path, modifiedContent });
