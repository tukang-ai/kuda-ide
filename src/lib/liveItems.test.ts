import { describe, expect, it } from 'vitest';
import { buildLiveItems } from './liveItems';
import type { UiMessage } from '../store/agent';

const user = (id: string, runId?: string): UiMessage => ({ id, role: 'user', content: id, runId });
const asst = (id: string, runId?: string): UiMessage => ({
  id,
  role: 'assistant',
  content: id,
  runId,
  streaming: false,
});

describe('buildLiveItems', () => {
  it('groups consecutive assistant messages sharing a runId into one run box', () => {
    const items = buildLiveItems([asst('a', 'r1'), asst('b', 'r1')]);
    expect(items).toHaveLength(1);
    expect(items[0]).toEqual({ type: 'run', runId: 'r1', messages: [asst('a', 'r1'), asst('b', 'r1')] });
  });

  it('keeps user prompts separate and splits runs at user messages', () => {
    const items = buildLiveItems([asst('a', 'r1'), user('u'), asst('b', 'r2')]);
    expect(items.map((i) => i.type)).toEqual(['run', 'user', 'run']);
    expect((items[0] as { type: 'run' }).type).toBe('run');
    expect((items[2] as { type: 'run'; runId: string }).runId).toBe('r2');
  });

  it('treats messages without a runId as their own run box', () => {
    const items = buildLiveItems([asst('a'), asst('b', 'r1'), asst('c')]);
    expect(items.map((i) => i.type)).toEqual(['run', 'run', 'run']);
  });
});
