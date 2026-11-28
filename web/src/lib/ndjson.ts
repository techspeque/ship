// Streamed NDJSON, as the daemon answers a logs request: one JSON value per
// line, blank keep-alive lines, and, when a log read fails, a body cut
// short with an error rather than ended cleanly.

import {
  API_VERSION,
  API_VERSION_HEADER,
  RequestError,
  isApiError,
  isLogLine,
  type LogLineView,
} from './api';

/**
 * How a stream ended: all of it was read; it was cut short (the daemon
 * could not read some logs, or the connection dropped), so what was shown
 * is not all of it; or the reader stopped it.
 */
export type StreamEnd = 'complete' | 'cut_short' | 'aborted';

/** Splits chunks into lines, keeping a partial last line for the next. */
export class LineSplitter {
  #rest = '';

  push(chunk: string): string[] {
    const parts = (this.#rest + chunk).split('\n');
    this.#rest = parts.pop() ?? '';
    return parts;
  }

  /** What is left after the last newline. */
  end(): string {
    const rest = this.#rest;
    this.#rest = '';
    return rest;
  }
}

/**
 * Reads `body` as NDJSON, calling `onValue` per non-blank line. A line that
 * is not JSON ends the read as `cut_short`, never skipped silently.
 */
export async function readNdjson(
  body: ReadableStream<Uint8Array>,
  onValue: (v: unknown) => boolean,
  signal?: AbortSignal,
): Promise<StreamEnd> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  const lines = new LineSplitter();
  const take = (line: string): boolean => {
    if (line.trim() === '') return true;
    let v: unknown;
    try {
      v = JSON.parse(line);
    } catch {
      return false;
    }
    return onValue(v);
  };
  try {
    for (;;) {
      let chunk: ReadableStreamReadResult<Uint8Array>;
      try {
        chunk = await reader.read();
      } catch {
        return signal?.aborted === true ? 'aborted' : 'cut_short';
      }
      if (chunk.done) break;
      for (const line of lines.push(decoder.decode(chunk.value, { stream: true }))) {
        if (!take(line)) return 'cut_short';
      }
    }
    return take(lines.end() + decoder.decode()) ? 'complete' : 'cut_short';
  } finally {
    void reader.cancel().catch(() => {});
  }
}

/**
 * Streams the logs at `path` (from [`logsPath`]), calling `onLine` per line.
 * An error answer is thrown as a [`RequestError`] with the daemon's message.
 */
export async function streamLogs(
  path: string,
  onLine: (l: LogLineView) => void,
  signal?: AbortSignal,
  fetchFn: typeof fetch = fetch,
): Promise<StreamEnd> {
  let response: Response;
  try {
    response = await fetchFn(path, {
      method: 'GET',
      headers: { [API_VERSION_HEADER]: API_VERSION, Accept: 'application/x-ndjson' },
      credentials: 'same-origin',
      cache: 'no-store',
      ...(signal ? { signal } : {}),
    });
  } catch {
    if (signal?.aborted === true) return 'aborted';
    throw new RequestError('The daemon did not answer.', null);
  }
  if (!response.ok) {
    let body: unknown = null;
    try {
      body = await response.json();
    } catch {
      // No JSON: the status is the message.
    }
    throw new RequestError(
      isApiError(body)
        ? `${body.message} (${body.code})`
        : `The daemon answered HTTP ${response.status}.`,
      response.status,
    );
  }
  if (response.body === null) return 'complete';
  return readNdjson(
    response.body,
    (v) => {
      if (!isLogLine(v)) return false;
      onLine(v);
      return true;
    },
    signal,
  );
}
