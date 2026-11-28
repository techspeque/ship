// Watching one operation for the operation view: its stored state (status,
// phase, failure) read from `GET /api/v1/operations/{id}`, kept current
// while its events arrive over SSE.
//
// The events carry the phase but not the status, so the stored operation is
// read again as it progresses (a queued operation shows running once it
// starts). The daemon sends the `finished` event before it stores the
// terminal state, so after `finished` the read is retried until a terminal
// status comes back, rather than showing whatever the first read found.

import type { OperationEventView, OperationStatus, OperationView } from './api';
import type { FollowEnd } from './sse';

const TERMINAL: readonly OperationStatus[] = ['succeeded', 'failed', 'interrupted'];

export function isTerminal(op: OperationView): boolean {
  return TERMINAL.includes(op.status);
}

export interface WatchOptions {
  /** Reads the stored operation; throws when it cannot. */
  read: () => Promise<OperationView>;
  /** Follows the events until `finished` or the signal aborts. */
  follow: (onEvent: (e: OperationEventView) => void, signal: AbortSignal) => Promise<FollowEnd>;
  /** Each stored state read, in order; never a non-terminal after a terminal one. */
  onOperation: (op: OperationView) => void;
  onEvent: (e: OperationEventView) => void;
  signal: AbortSignal;
  /** First wait between terminal reads, doubled up to `maxRetryMs`. */
  retryMs?: number;
  maxRetryMs?: number;
  /** Terminal reads before giving up with an error. */
  maxReads?: number;
  /** Waits `ms`; replaceable in tests. */
  sleep?: (ms: number, signal: AbortSignal) => Promise<void>;
}

function defaultSleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms);
    signal.addEventListener(
      'abort',
      () => {
        clearTimeout(timer);
        resolve();
      },
      { once: true },
    );
  });
}

export const NOT_TERMINAL =
  'The operation finished, but its final status could not be read. Reload the page.';

/**
 * Reads operation state, follows its events and reads it again until it is
 * stored as ended. Resolves when it is, or when the signal aborts; rejects
 * when the first read fails, the follow fails, or the terminal state cannot
 * be read after `maxReads` tries. A failed read during progress is skipped:
 * the next event or the terminal read tries again.
 */
export async function watchOperation(options: WatchOptions): Promise<void> {
  const { read, follow, onOperation, onEvent, signal } = options;
  const sleep = options.sleep ?? defaultSleep;
  const base = options.retryMs ?? 250;
  const max = options.maxRetryMs ?? 2_000;
  const maxReads = options.maxReads ?? 10;
  const aborted = (): boolean => signal.aborted;

  let current = await read();
  if (aborted()) return;
  const apply = (op: OperationView): void => {
    if (aborted() || (isTerminal(current) && !isTerminal(op))) return;
    current = op;
    onOperation(op);
  };
  onOperation(current);

  // One progress read at a time; a request while one runs reads once more
  // after it, so the last answer applied is never older than the last event.
  let running: Promise<void> | null = null;
  let again = false;
  const refresh = (): void => {
    if (running !== null) {
      again = true;
      return;
    }
    running = (async () => {
      do {
        again = false;
        try {
          apply(await read());
        } catch {
          // Skipped: the next event or the terminal read tries again.
        }
      } while (again && !aborted());
      running = null;
    })();
  };

  const end = await follow((e) => {
    onEvent(e);
    if (e.event === 'finished') return;
    if (e.event !== 'log' || current.status === 'queued') refresh();
  }, signal);
  if (end !== 'finished' || aborted()) return;
  await running;

  let wait = base;
  let lastError: unknown = null;
  for (let n = 0; n < maxReads && !aborted(); n++) {
    if (isTerminal(current)) return;
    if (n > 0) {
      await sleep(wait, signal);
      wait = Math.min(wait * 2, max);
      if (aborted()) return;
    }
    try {
      apply(await read());
      lastError = null;
    } catch (e) {
      lastError = e;
    }
  }
  if (aborted() || isTerminal(current)) return;
  throw lastError instanceof Error ? lastError : new Error(NOT_TERMINAL);
}
