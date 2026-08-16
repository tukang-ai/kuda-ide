export interface FlushedText {
  msgId: string;
  text: string;
}

/**
 * Coalesces streaming text deltas for a single active agent run.
 *
 * The backend sends one ThoughtDelta per chunk; applying each as its own
 * Zustand set floods the UI thread with re-renders. This buffer accumulates
 * deltas per message id so the store can flush at most once per animation
 * frame instead of once per delta.
 */
export class StreamTextBuffer {
  private pending: FlushedText | null = null;

  /**
   * Append `text` to `msgId`. When the active message changes mid-run the
   * previously buffered text is returned so the caller can flush it before
   * buffering the new message's text.
   */
  enqueue(msgId: string, text: string): FlushedText | null {
    if (this.pending && this.pending.msgId !== msgId) {
      const overflow = this.pending;
      this.pending = { msgId, text };
      return overflow;
    }
    if (this.pending) {
      this.pending.text += text;
    } else {
      this.pending = { msgId, text };
    }
    return null;
  }

  /** Take and clear the pending text (returns null when empty). */
  drain(): FlushedText | null {
    const pending = this.pending;
    this.pending = null;
    return pending;
  }

  clear(): void {
    this.pending = null;
  }
}
