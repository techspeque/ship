import { untrack } from 'svelte';

/**
 * Runs `work` now and then every `ms`, one run at a time. `refreshing` is
 * reactive, for the Refresh button.
 *
 * `start` reads `refreshing` through `refresh`, so it runs untracked: called
 * from an effect, a tracked read would rerun the effect, and fetch again, each
 * time a run ends, instead of waiting for the interval.
 */
export class Poller {
  refreshing = $state(false);
  readonly #work: () => Promise<void>;

  constructor(work: () => Promise<void>) {
    this.#work = work;
  }

  async refresh(): Promise<void> {
    if (this.refreshing) return;
    this.refreshing = true;
    try {
      await this.#work();
    } finally {
      this.refreshing = false;
    }
  }

  /** Starts polling; the returned function stops it. */
  start(ms: number): () => void {
    untrack(() => void this.refresh());
    const timer = setInterval(() => void this.refresh(), ms);
    return () => clearInterval(timer);
  }
}
