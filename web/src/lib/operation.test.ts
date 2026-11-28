import { describe, expect, it } from 'vitest';

import type { OperationEventView, OperationStatus, OperationView } from './api';
import { NOT_TERMINAL, watchOperation } from './operation';
import type { FollowEnd } from './sse';

const OP = 'op_01aryz6s41tsv4rrffq69g5fav';

function op(status: OperationStatus, phase: OperationView['phase']): OperationView {
  return {
    id: OP,
    kind: 'deploy',
    environment_id: null,
    phase,
    status,
    failure:
      status === 'failed' ? { code: 'build_failed', phase: 'building', message: 'exit 1' } : null,
    log_bytes: 0,
    created_at: '2026-01-02T03:04:05.000Z',
    updated_at: '2026-01-02T03:04:05.000Z',
  };
}

function ev(
  sequence: number,
  event: string,
  phase: OperationEventView['phase'],
): OperationEventView {
  return { operation_id: OP, sequence, phase, event, message: '', ts: '2026-01-02T03:04:05.000Z' };
}

/** Reads answered from `answers` in order; the last one repeats. */
function reads(answers: (OperationView | Error)[]) {
  let n = 0;
  const read = (): Promise<OperationView> => {
    const a = answers[Math.min(n, answers.length - 1)];
    n++;
    if (a === undefined) throw new Error('no answers');
    return a instanceof Error ? Promise.reject(a) : Promise.resolve(a);
  };
  return { read, count: () => n };
}

/** A follow delivering `events` one per tick, then `finished`. */
function follow(events: OperationEventView[], end: FollowEnd = 'finished') {
  return async (onEvent: (e: OperationEventView) => void): Promise<FollowEnd> => {
    for (const e of events) {
      await Promise.resolve();
      onEvent(e);
      // Let a progress read settle before the next event.
      await new Promise((r) => setTimeout(r, 0));
    }
    return end;
  };
}

const noSleep = (): Promise<void> => Promise.resolve();

describe('watchOperation', () => {
  it('shows a queued operation as running once it starts', async () => {
    const r = reads([op('queued', 'queued'), op('running', 'building'), op('succeeded', 'succeeded')]);
    const seen: OperationStatus[] = [];
    await watchOperation({
      read: r.read,
      follow: follow([ev(1, 'phase_started', 'building'), ev(2, 'finished', 'succeeded')]),
      onOperation: (o) => seen.push(o.status),
      onEvent: () => {},
      signal: new AbortController().signal,
      sleep: noSleep,
    });
    expect(seen).toEqual(['queued', 'running', 'succeeded']);
  });

  it('reads again until the terminal state is stored after finished', async () => {
    // `finished` is sent before the daemon stores the end: the first reads
    // after it still find the operation running.
    const r = reads([
      op('queued', 'queued'),
      op('running', 'building'),
      op('running', 'building'),
      new Error('The daemon did not answer.'),
      op('failed', 'failed'),
    ]);
    const seen: OperationView[] = [];
    const waits: number[] = [];
    await watchOperation({
      read: r.read,
      follow: follow([ev(1, 'phase_started', 'building'), ev(2, 'finished', 'failed')]),
      onOperation: (o) => seen.push(o),
      onEvent: () => {},
      signal: new AbortController().signal,
      sleep: (ms) => {
        waits.push(ms);
        return Promise.resolve();
      },
    });
    const last = seen[seen.length - 1];
    expect(last?.status).toBe('failed');
    expect(last?.failure).toEqual({ code: 'build_failed', phase: 'building', message: 'exit 1' });
    expect(waits).toEqual([250, 500]);
  });

  it('gives up with an error when the terminal state never appears', async () => {
    const r = reads([op('running', 'building')]);
    await expect(
      watchOperation({
        read: r.read,
        follow: follow([ev(1, 'finished', 'succeeded')]),
        onOperation: () => {},
        onEvent: () => {},
        signal: new AbortController().signal,
        sleep: noSleep,
        maxReads: 3,
      }),
    ).rejects.toThrow(NOT_TERMINAL);
    expect(r.count()).toBe(4);
  });

  it('stops reading once the terminal state is stored', async () => {
    const r = reads([op('running', 'building'), op('succeeded', 'succeeded')]);
    const seen: OperationStatus[] = [];
    await watchOperation({
      read: r.read,
      follow: follow([ev(1, 'finished', 'succeeded')]),
      onOperation: (o) => seen.push(o.status),
      onEvent: () => {},
      signal: new AbortController().signal,
      sleep: noSleep,
    });
    expect(seen).toEqual(['running', 'succeeded']);
    expect(r.count()).toBe(2);
  });

  it('stops reading once aborted', async () => {
    const stop = new AbortController();
    const r = reads([op('running', 'building')]);
    const seen: OperationStatus[] = [];
    await watchOperation({
      read: r.read,
      follow: (onEvent) => {
        onEvent(ev(1, 'finished', 'succeeded'));
        stop.abort();
        return Promise.resolve('aborted');
      },
      onOperation: (o) => seen.push(o.status),
      onEvent: () => {},
      signal: stop.signal,
      sleep: noSleep,
    });
    expect(r.count()).toBe(1);
    expect(seen).toEqual(['running']);
  });

  it('rejects when the first read fails', async () => {
    await expect(
      watchOperation({
        read: () => Promise.reject(new Error('no such operation')),
        follow: follow([]),
        onOperation: () => {},
        onEvent: () => {},
        signal: new AbortController().signal,
      }),
    ).rejects.toThrow('no such operation');
  });
});
