import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { Poller } from './poller.svelte';

const POLL_MS = 10_000;

async function turns(n: number): Promise<void> {
  for (let i = 0; i < n; i++) await Promise.resolve();
}

describe('Poller', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  // f-009: started from an effect, the end of each run must not rerun the
  // effect; the second request waits for the interval.
  it('sends no second request before the polling interval', async () => {
    let calls = 0;
    const poller = new Poller(() => {
      calls += 1;
      return Promise.resolve();
    });
    const cleanup = $effect.root(() => {
      $effect(() => poller.start(POLL_MS));
    });
    try {
      await turns(30);
      expect(calls).toBe(1);
      expect(poller.refreshing).toBe(false);

      await vi.advanceTimersByTimeAsync(POLL_MS - 1);
      await turns(30);
      expect(calls).toBe(1);

      await vi.advanceTimersByTimeAsync(1);
      await turns(30);
      expect(calls).toBe(2);
    } finally {
      cleanup();
    }
  });

  it('runs one refresh at a time', async () => {
    let calls = 0;
    let release = (): void => undefined;
    const poller = new Poller(() => {
      calls += 1;
      return new Promise<void>((resolve) => {
        release = resolve;
      });
    });
    const first = poller.refresh();
    expect(poller.refreshing).toBe(true);
    await poller.refresh();
    expect(calls).toBe(1);
    release();
    await first;
    expect(poller.refreshing).toBe(false);
  });

  it('stops polling when stopped', async () => {
    let calls = 0;
    const poller = new Poller(() => {
      calls += 1;
      return Promise.resolve();
    });
    const stop = poller.start(POLL_MS);
    await turns(5);
    stop();
    await vi.advanceTimersByTimeAsync(3 * POLL_MS);
    expect(calls).toBe(1);
  });
});
