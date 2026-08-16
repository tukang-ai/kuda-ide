import { describe, expect, it } from 'vitest';
import { StreamTextBuffer } from './streamBuffer';

describe('StreamTextBuffer', () => {
  it('accumulates consecutive deltas for the same message', () => {
    const buffer = new StreamTextBuffer();
    expect(buffer.enqueue('m1', 'hel')).toBeNull();
    expect(buffer.enqueue('m1', 'lo')).toBeNull();
    expect(buffer.drain()).toEqual({ msgId: 'm1', text: 'hello' });
    expect(buffer.drain()).toBeNull();
  });

  it('returns the previous message text when the active message changes', () => {
    const buffer = new StreamTextBuffer();
    buffer.enqueue('m1', 'old');
    const overflow = buffer.enqueue('m2', 'new');
    expect(overflow).toEqual({ msgId: 'm1', text: 'old' });
    expect(buffer.drain()).toEqual({ msgId: 'm2', text: 'new' });
  });

  it('drains null when empty', () => {
    expect(new StreamTextBuffer().drain()).toBeNull();
  });

  it('clear empties any pending text', () => {
    const buffer = new StreamTextBuffer();
    buffer.enqueue('m1', 'x');
    buffer.clear();
    expect(buffer.drain()).toBeNull();
  });

  it('flush stays pending after enqueueing onto a new message', () => {
    const buffer = new StreamTextBuffer();
    const overflow = buffer.enqueue('m1', 'a');
    expect(overflow).toBeNull();
    expect(buffer.enqueue('m2', 'b')).toEqual({ msgId: 'm1', text: 'a' });
    expect(buffer.enqueue('m2', 'c')).toBeNull();
    expect(buffer.drain()).toEqual({ msgId: 'm2', text: 'bc' });
  });
});
