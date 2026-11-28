import { describe, expect, it } from 'vitest';

import { RequestError, type OperationEventView } from './api';
import { SseParser, followOperation, type Connection } from './sse';

const OP = 'op_01aryz6s41tsv4rrffq69g5fav';

function event(sequence: number, last: number): OperationEventView {
  return {
    operation_id: OP,
    sequence,
    phase: sequence === last ? 'succeeded' : 'deploying',
    event: sequence === last ? 'finished' : 'log',
    message: `line ${sequence}`,
    ts: '2026-01-02T03:04:05.000Z',
  };
}

function frame(e: OperationEventView): string {
  return `id: ${e.sequence}\ndata: ${JSON.stringify(e)}\n\n`;
}

/** `text` as a byte stream in chunks of `size`, then ended or errored. */
function body(text: string, size: number, end: 'close' | 'error'): ReadableStream<Uint8Array> {
  const bytes = new TextEncoder().encode(text);
  let at = 0;
  return new ReadableStream({
    pull(controller) {
      if (at < bytes.length) {
        controller.enqueue(bytes.slice(at, at + size));
        at += size;
      } else if (end === 'close') {
        controller.close();
      } else {
        controller.error(new TypeError('network error'));
      }
    },
  });
}

/** One connection's plan: which sequences to send, and how it ends. */
interface Plan {
  /** Sends from `after - overlap + 1`, repeating `overlap` already sent. */
  overlap?: number;
  /** The last sequence this connection sends. */
  upTo: number;
  end: 'close' | 'error';
  /** Sequences to leave out (a gap). */
  skip?: number[];
}

/** A fake daemon serving `total` events over the given connections. */
function daemon(total: number, plans: Plan[]) {
  const resumedFrom: (string | null)[] = [];
  const fetchFn = ((_url: string, init?: RequestInit) => {
    const after = new Headers(init?.headers).get('last-event-id');
    resumedFrom.push(after);
    const plan = plans[resumedFrom.length - 1] ?? { upTo: total, end: 'close' };
    const from = Math.max(1, Number(after ?? 0) + 1 - (plan.overlap ?? 0));
    let text = ': keep-alive\n\n';
    for (let s = from; s <= Math.min(plan.upTo, total); s++) {
      if (plan.skip?.includes(s) === true) continue;
      text += frame(event(s, total));
    }
    return Promise.resolve(
      new Response(body(text, 7, plan.end), {
        status: 200,
        headers: { 'content-type': 'text/event-stream' },
      }),
    );
  }) as typeof fetch;
  return { fetchFn, resumedFrom };
}

const now = (): Promise<void> => Promise.resolve();

describe('SseParser', () => {
  it('parses frames split anywhere, with comments, CRLF and multi-line data', () => {
    const text = ': hello\r\nid: 1\r\ndata: a\r\ndata: b\r\n\r\nid: 2\ndata:c\n\n\n';
    for (let size = 1; size <= text.length; size++) {
      const p = new SseParser();
      const frames = [];
      for (let i = 0; i < text.length; i += size) frames.push(...p.push(text.slice(i, i + size)));
      expect(frames, `chunk ${size}`).toEqual([
        { id: '1', data: 'a\nb' },
        { id: '2', data: 'c' },
      ]);
    }
  });

  it('keeps the last id for a frame that does not set one', () => {
    const p = new SseParser();
    expect(p.push('id: 4\ndata: x\n\ndata: y\n\n')).toEqual([
      { id: '4', data: 'x' },
      { id: '4', data: 'y' },
    ]);
  });
});

describe('followOperation', () => {
  it('resumes after dropped connections with no event lost or repeated', async () => {
    const total = 12;
    const { fetchFn, resumedFrom } = daemon(total, [
      { upTo: 3, end: 'error' },
      // The daemon may resend events already seen; they must not repeat.
      { upTo: 7, end: 'close', overlap: 2 },
      { upTo: 9, end: 'error', overlap: 1 },
    ]);
    const seen: number[] = [];
    const states: Connection[] = [];
    const end = await followOperation(OP, (e) => seen.push(e.sequence), {
      fetchFn,
      sleep: now,
      onConnection: (c) => states.push(c),
    });
    expect(end).toBe('finished');
    expect(seen).toEqual(Array.from({ length: total }, (_, i) => i + 1));
    expect(resumedFrom).toEqual([null, '3', '7', '9']);
    expect(states.filter((s) => s === 'reconnecting').length).toBe(3);
    expect(states.at(-1)).toBe('ended');
  });

  it('drops a connection with a gap and resumes from the last event applied', async () => {
    const { fetchFn, resumedFrom } = daemon(6, [{ upTo: 6, end: 'close', skip: [3] }]);
    const seen: number[] = [];
    await followOperation(OP, (e) => seen.push(e.sequence), { fetchFn, sleep: now });
    expect(seen).toEqual([1, 2, 3, 4, 5, 6]);
    expect(resumedFrom).toEqual([null, '2']);
  });

  it('resumes from `after`, for events already shown', async () => {
    const { fetchFn, resumedFrom } = daemon(5, []);
    const seen: number[] = [];
    await followOperation(OP, (e) => seen.push(e.sequence), { fetchFn, sleep: now, after: 2 });
    expect(seen).toEqual([3, 4, 5]);
    expect(resumedFrom).toEqual(['2']);
  });

  it('retries a 5xx and a network failure, and stops at the finished event', async () => {
    let calls = 0;
    const inner = daemon(2, []);
    const fetchFn = ((url: string, init?: RequestInit) => {
      calls += 1;
      if (calls === 1) return Promise.reject(new TypeError('offline'));
      if (calls === 2) return Promise.resolve(new Response('{}', { status: 503 }));
      return inner.fetchFn(url, init);
    }) as typeof fetch;
    const seen: number[] = [];
    expect(await followOperation(OP, (e) => seen.push(e.sequence), { fetchFn, sleep: now })).toBe(
      'finished',
    );
    expect(seen).toEqual([1, 2]);
    expect(calls).toBe(3);
  });

  it('throws the daemon message for a 4xx', async () => {
    const fetchFn = (() =>
      Promise.resolve(
        new Response(
          JSON.stringify({
            code: 'not_found',
            message: 'no such operation',
            details: {},
            operation_id: null,
          }),
          { status: 404 },
        ),
      )) as typeof fetch;
    await expect(followOperation(OP, () => {}, { fetchFn, sleep: now })).rejects.toThrow(
      new RequestError('no such operation (not_found)', 404),
    );
  });

  it('ends `aborted` when the signal fires', async () => {
    const stop = new AbortController();
    const { fetchFn } = daemon(10, [{ upTo: 2, end: 'close' }]);
    const seen: number[] = [];
    const end = await followOperation(
      OP,
      (e) => {
        seen.push(e.sequence);
        if (e.sequence === 2) stop.abort();
      },
      { fetchFn, sleep: now, signal: stop.signal },
    );
    expect(end).toBe('aborted');
    expect(seen).toEqual([1, 2]);
  });

  it('sends the API version and asks for an event stream', async () => {
    let headers: Headers | null = null;
    const inner = daemon(1, []);
    const fetchFn = ((url: string, init?: RequestInit) => {
      headers = new Headers(init?.headers);
      expect(url).toBe(`/api/v1/operations/${OP}/events`);
      expect(init?.method).toBe('GET');
      expect(init?.body).toBeUndefined();
      return inner.fetchFn(url, init);
    }) as typeof fetch;
    await followOperation(OP, () => {}, { fetchFn, sleep: now });
    expect(headers).not.toBeNull();
    const h = headers as unknown as Headers;
    expect(h.get('ship-api-version')).toBe('v1');
    expect(h.get('accept')).toBe('text/event-stream');
  });
});
