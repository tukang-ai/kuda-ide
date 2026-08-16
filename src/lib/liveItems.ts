import type { UiMessage } from '../store/agent';

export type LiveRenderItem =
  | { type: 'user'; msg: UiMessage }
  | { type: 'run'; runId: string; messages: UiMessage[] };

/**
 * Orders the live stream: user prompts stay separate (their own bubble), while
 * all assistant sections sharing a run id collapse into a single agent box.
 * Runs are sequential, so consecutive messages with the same runId are grouped.
 */
export function buildLiveItems(msgs: UiMessage[]): LiveRenderItem[] {
  const items: LiveRenderItem[] = [];
  let currentRun: { runId: string; messages: UiMessage[] } | null = null;
  for (const m of msgs) {
    if (m.role === 'user') {
      currentRun = null;
      items.push({ type: 'user', msg: m });
      continue;
    }
    const key = m.runId ?? m.id;
    if (currentRun && currentRun.runId === key) {
      currentRun.messages.push(m);
    } else {
      currentRun = { runId: key, messages: [m] };
      items.push({ type: 'run', runId: key, messages: currentRun.messages });
    }
  }
  return items;
}
