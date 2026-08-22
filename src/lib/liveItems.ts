import type { UiMessage } from '../store/agent';

export type LiveRenderItem =
  | { type: 'user'; msg: UiMessage }
  | { type: 'run'; runId: string; /** Unique per GROUP (raw run ids can repeat after resuming an old run) */ groupKey: string; messages: UiMessage[] };

/**
 * Orders the live stream: user prompts stay separate (their own bubble), while
 * all assistant sections sharing a run id collapse into a single agent box.
 * Runs are sequential, so consecutive messages with the same runId are grouped.
 */
export function buildLiveItems(msgs: UiMessage[]): LiveRenderItem[] {
  const items: LiveRenderItem[] = [];
  let currentRun: { groupKey: string; messages: UiMessage[] } | null = null;
  let occurrence = 0;
  for (const m of msgs) {
    if (m.role === 'user') {
      currentRun = null;
      items.push({ type: 'user', msg: m });
      continue;
    }
    const key = m.runId ?? m.id;
    if (currentRun && currentRun.groupKey.startsWith(`${key}#`)) {
      currentRun.messages.push(m);
    } else {
      // Resuming an OLD run appends new boxes carrying that old run id AFTER
      // newer runs — two groups would then share one raw id, so the group key
      // gets an occurrence suffix while `runId` stays the raw value.
      const groupKey = `${key}#${occurrence++}`;
      currentRun = { groupKey, messages: [m] };
      items.push({ type: 'run', runId: key, groupKey, messages: currentRun.messages });
    }
  }
  return items;
}
