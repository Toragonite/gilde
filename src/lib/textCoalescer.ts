/**
 * Merges text chunks per key into one sink call per `flushMs`, so a fast
 * stream (conversation deltas, live-region text) causes one store write per
 * window instead of one per chunk. `flushNow()` makes it testable without timers.
 */

export class TextCoalescer<K> {
  private pending = new Map<K, string>();
  private timer: ReturnType<typeof setTimeout> | null = null;

  constructor(
    private readonly sink: (batch: Map<K, string>) => void,
    private readonly flushMs: number = 100,
  ) {}

  push(key: K, chunk: string): void {
    if (!chunk) return;
    this.pending.set(key, (this.pending.get(key) ?? '') + chunk);
    if (this.timer) return;
    this.timer = setTimeout(() => {
      this.timer = null;
      this.flushNow();
    }, this.flushMs);
  }

  /** Number of keys with unflushed text. */
  get pendingSize(): number {
    return this.pending.size;
  }

  flushNow(): void {
    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = null;
    }
    if (this.pending.size === 0) return;
    const batch = this.pending;
    this.pending = new Map();
    this.sink(batch);
  }

  stop(): void {
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    this.pending = new Map();
  }
}
