// Following an operation's events over SSE (ADR-0011), resuming after a
// dropped connection.
//
// `fetch` and a small parser rather than `EventSource`: an `EventSource`
// reconnects on its own after the daemon ends the stream at the terminal
// event, so it would ask again forever, and it cannot run in the unit tests.
// Here every reconnect sends `Last-Event-ID` with the last sequence applied,
// and an event at or below it is dropped, so a line is never shown twice;
// the daemon resends everything after it, so none is lost.

import {
  API_VERSION,
  API_VERSION_HEADER,
  LAST_EVENT_ID_HEADER,
  RequestError,
  isApiError,
  isOperationEvent,
  operationEventsPath,
  type OperationEventView,
} from './api';

/** One dispatched SSE event: its last `id:` and its `data:` lines joined. */
export interface SseFrame {
  id: string | null;
  data: string;
}

/**
 * Incremental `text/event-stream` parser (the WHATWG rules this daemon's
 * frames need): lines end in LF, CRLF or CR, possibly split across chunks;
 * `:` lines are comments; a blank line dispatches the event when it has
 * data; one space after the colon is dropped.
 */
export class SseParser {
  #buffer = '';
  #data: string[] = [];
  #id: string | null = null;

  push(chunk: string): SseFrame[] {
    this.#buffer += chunk;
    const frames: SseFrame[] = [];
    for (;;) {
      const m = /\r\n|\n|\r/.exec(this.#buffer);
      if (m === null) break;
      // A CR at the very end may be the first half of a CRLF.
      if (m[0] === '\r' && m.index === this.#buffer.length - 1) break;
      const line = this.#buffer.slice(0, m.index);
      this.#buffer = this.#buffer.slice(m.index + m[0].length);
      const frame = this.#line(line);
      if (frame !== null) frames.push(frame);
    }
    return frames;
  }

  #line(line: string): SseFrame | null {
    if (line === '') {
      if (this.#data.length === 0) return null;
      const frame = { id: this.#id, data: this.#data.join('\n') };
      this.#data = [];
      return frame;
    }
    if (line.startsWith(':')) return null;
    const colon = line.indexOf(':');
    const field = colon === -1 ? line : line.slice(0, colon);
    let value = colon === -1 ? '' : line.slice(colon + 1);
    if (value.startsWith(' ')) value = value.slice(1);
    if (field === 'data') this.#data.push(value);
    else if (field === 'id' && !value.includes('\0')) this.#id = value;
    return null;
  }
}

/** The connection as the page shows it. */
export type Connection = 'connecting' | 'live' | 'reconnecting' | 'ended';

export interface FollowOptions {
  fetchFn?: typeof fetch;
  /** Ends the follow; the promise then resolves `aborted`. */
  signal?: AbortSignal;
  /** First wait before a reconnect, doubled up to `maxRetryMs`. */
  retryMs?: number;
  maxRetryMs?: number;
  /** Resume after this sequence (the events already shown). */
  after?: number;
  onConnection?: (c: Connection) => void;
  /** Waits `ms`; replaceable in tests. */
  sleep?: (ms: number, signal?: AbortSignal) => Promise<void>;
}

export type FollowEnd = 'finished' | 'aborted';

function defaultSleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms);
    signal?.addEventListener(
      'abort',
      () => {
        clearTimeout(timer);
        resolve();
      },
      { once: true },
    );
  });
}

/**
 * Follows operation `id`'s events from `after`, calling `onEvent` once per
 * sequence, in order, until the `finished` event. A dropped or ended
 * connection is reopened with `Last-Event-ID`. An answer the daemon refuses
 * with a `4xx` (no such operation) is thrown as a [`RequestError`]; a `5xx`
 * or a network failure is retried.
 */
export async function followOperation(
  id: string,
  onEvent: (e: OperationEventView) => void,
  options: FollowOptions = {},
): Promise<FollowEnd> {
  const fetchFn = options.fetchFn ?? fetch;
  const sleep = options.sleep ?? defaultSleep;
  const base = options.retryMs ?? 500;
  const max = options.maxRetryMs ?? 5_000;
  const signal = options.signal;
  const say = options.onConnection ?? (() => {});
  let last = options.after ?? 0;
  let wait = base;
  // A function, so the checks after each `await` are not narrowed away.
  const aborted = (): boolean => signal?.aborted === true;

  say('connecting');
  while (!aborted()) {
    const headers: Record<string, string> = {
      [API_VERSION_HEADER]: API_VERSION,
      Accept: 'text/event-stream',
    };
    if (last > 0) headers[LAST_EVENT_ID_HEADER] = String(last);
    let response: Response | null;
    try {
      response = await fetchFn(operationEventsPath(id), {
        method: 'GET',
        headers,
        credentials: 'same-origin',
        cache: 'no-store',
        ...(signal ? { signal } : {}),
      });
    } catch {
      response = null;
    }
    if (aborted()) break;
    if (response !== null && response.status >= 400 && response.status < 500) {
      let body: unknown = null;
      try {
        body = await response.json();
      } catch {
        // No JSON: the status is the message.
      }
      say('ended');
      throw new RequestError(
        isApiError(body)
          ? `${body.message} (${body.code})`
          : `The daemon answered HTTP ${response.status}.`,
        response.status,
      );
    }
    if (response !== null && response.ok && response.body !== null) {
      say('live');
      const parser = new SseParser();
      const decoder = new TextDecoder();
      const reader = response.body.getReader();
      let finished = false;
      let broken = false;
      try {
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          for (const frame of parser.push(decoder.decode(value, { stream: true }))) {
            let parsed: unknown;
            try {
              parsed = JSON.parse(frame.data);
            } catch {
              parsed = null;
            }
            if (!isOperationEvent(parsed)) {
              broken = true;
              break;
            }
            if (parsed.sequence <= last) continue;
            // The daemon sends every sequence; a gap means this connection
            // lost something, so it is dropped and resumed from `last`.
            if (parsed.sequence !== last + 1) {
              broken = true;
              break;
            }
            last = parsed.sequence;
            wait = base;
            onEvent(parsed);
            if (parsed.event === 'finished') {
              finished = true;
              break;
            }
          }
          if (finished || broken) break;
        }
      } catch {
        // A dropped connection: resume below.
      } finally {
        void reader.cancel().catch(() => {});
      }
      if (finished) {
        say('ended');
        return 'finished';
      }
    }
    if (aborted()) break;
    say('reconnecting');
    await sleep(wait, signal);
    wait = Math.min(wait * 2, max);
  }
  say('ended');
  return 'aborted';
}
