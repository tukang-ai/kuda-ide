export interface DirEntryItem {
  name: string;
  path: string;
  is_dir: boolean;
}

export interface FileContentPayload {
  path: string;
  content: string;
}

export interface SearchQuery {
  pattern: string;
  is_regex: boolean;
  case_sensitive: boolean;
  max_results?: number;
}

export interface SearchMatch {
  file_path: string;
  line_number: number;
  line_content: string;
}

export interface CodeSymbol {
  name: string;
  kind: SymbolKind;
  file_path: string;
  start_line: number;
  end_line: number;
  signature?: string;
}

export type SymbolKind =
  | 'Function' | 'Method' | 'Class' | 'Struct'
  | 'Interface' | 'Enum' | 'Variable' | 'Module';

export interface TerminalOutputPayload {
  session_id: string;
  data: string;
  is_base64: boolean;
}

export interface WriteFileResponse {
  path: string;
  checkpoint_id: string | null;
}

export interface ProjectInfo {
  root: string;
  name: string;
  app_data_dir: string;
}

export interface ChatSessionMeta {
  session_id: string;
  title: string;
  created_at: string;
  updated_at: string;
  message_count: number;
}

export type MessageRole = 'System' | 'User' | 'Assistant' | 'Tool';

export interface ChatMessage {
  role: MessageRole;
  content: string;
  name?: string | null;
  tool_calls?: ToolCallChunk[] | null;
}

export interface ToolCallChunk {
  call_id: string;
  tool_name: string;
  arguments_json: string;
}

export interface PhaseToolCall {
  call_id: string;
  tool_name: string;
  arguments_json: string;
  output: string;
  status: string;
}

export interface PhaseRecord {
  run_id: string;
  role: string;
  label: string;
  model: string;
  summary: string;
  text: string;
  thinking?: string;
  tool_calls: PhaseToolCall[];
  created_at: string;
}

export interface ChatSessionData {
  meta: ChatSessionMeta;
  messages: ChatMessage[];
  transcript?: PhaseRecord[];
  checkpoint_ids: string[];
}

export interface FileCheckpoint {
  checkpoint_id: string;
  original_file_path: string;
  backup_file_path: string;
  original_sha256: string;
  timestamp: string;
  agent_message_id?: string | null;
  session_id?: string | null;
  existed_before: boolean;
}

export interface SessionInfo {
  session_id: string;
  timestamp: string;
  file_count: number;
  files: string[];
}

export interface AgentRunResult {
  chat_session_id: string;
  edit_session_id: string | null;
}

export type AgentRoleKey =
  | 'thinker'
  | 'reviewer'
  | 'planning_writer'
  | 'plan_reviewer'
  | 'plan_editor'
  | 'executor_code'
  | 'executor_design'
  | 'executor_reviewer'
  | 'rlm_model'
  | 'rlm_verifier';

export type AgentEvent =
  | { kind: { ThoughtDelta: string } }
  | { kind: { ThinkingDelta: string } }
  | { kind: { ToolCallStarted: { tool_name: string; call_id: string; arguments_json: string } } }
  | { kind: { ToolCallCompleted: { tool_name: string; call_id: string; output: string } } }
  | {
      kind: {
        Finished: {
          total_tokens_used: number
          tokens_in: number
          tokens_out: number
          cached_in: number
        }
      }
    }
  | { kind: { Error: string } }
  | { kind: { PhaseStarted: { role: AgentRoleKey; label: string; model: string } } }
  | {
      kind: {
        PhaseCompleted: {
          role: AgentRoleKey
          summary: string
          tokens_in: number
          tokens_out: number
          cached_in: number
        }
      }
    }
  | { kind: { ExternalAccessRequest: { request_id: string; path: string; reason: string; kind: string } } }
  | { kind: { ExternalAccessResolved: { request_id: string; allowed: boolean } } }
  | {
      kind: {
        PlanDecisionRequest: {
          request_id: string;
          plan_markdown: string;
          plan_file_path: string;
          round: number;
          tasks_count: number;
          latest_note: string | null;
        };
      };
    }
  | { kind: { PlanDecisionResolved: { request_id: string; decision: string; note: string | null } } }
  | { kind: { DirectionDecisionRequest: { request_id: string; conclusion: string } } }
  | { kind: { DirectionDecisionResolved: { request_id: string; decision: string; note: string | null } } };

export type ChangeKind = 'Insert' | 'Delete' | 'Equal';

export interface DiffChange {
  kind: ChangeKind;
  content: string;
  old_line?: number | null;
  new_line?: number | null;
}

export interface DiffResult {
  file_path: string;
  original_content: string;
  modified_content: string;
  changes: DiffChange[];
  insertions: number;
  deletions: number;
}

export interface OpenTab {
  path: string;
  name: string;
  content: string;
  savedContent: string;
  language: string;
}
