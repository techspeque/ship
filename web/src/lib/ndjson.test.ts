import { describe, expect, it } from 'vitest';

import { RequestError, logsPath, type LogLineView } from './api';
import { LineSplitter, readNdjson, streamLogs } from './ndjson';

function stream(chunks: string[], end: 'close' | 'error'): ReadableStream<Uint8Array> {
  const enc = new TextEncoder();
  let i = 0;
  return new ReadableStream({
    pull(c) {
      const chunk = chunks[i++];
      if (chunk !== undefined) c.enqueue(enc.encode(chunk));
      else if (end === 'close') c.close();
      else c.error(new TypeError('terminated'));
    },
  });
}

const line = (service: string, stream: string, text: string): LogLineView => ({
  service,
  stream,
  ts: '2026-01-02T03:04:46.120Z',
  line: text,
});

const ndjson = (lines: LogLineView[]): string => lines.map((l) => JSON.stringify(l) + '\n').join('');

describe('LineSplitter', () => {
  it('keeps a partial line for the next chunk', () => {
    const s = new LineSplitter();
    expect(s.push('a\nb')).toEqual(['a']);
    expect(s.push('c\n\nd')).toEqual(['bc', '']);
    expect(s.end()).toBe('d');
  });
});

describe('readNdjson', () => {
  const lines = [
    line('web', 'stdout', 'listening on :8080'),
    line('web', 'stderr', 'warning: no cache directory'),
    line('worker', 'stdout', 'ready ✓'),
  ];
  const text = ndjson(lines);

  it('reads every line across any chunking, skipping keep-alive blank lines', async () => {
    const withKeepAlive = text.replace('\n', '\n\n\n');
    for (const size of [1, 3, 17, withKeepAlive.length]) {
      const chunks = [];
      for (let i = 0; i < withKeepAlive.length; i += size) chunks.push(withKeepAlive.slice(i, i + size));
      const got: unknown[] = [];
      const end = await readNdjson(stream(chunks, 'close'), (v) => (got.push(v), true));
      expect(end, `size ${size}`).toBe('complete');
      expect(got).toEqual(lines);
    }
  });

  it('a body that ends with an error is cut short, not complete', async () => {
    const got: unknown[] = [];
    const end = await readNdjson(stream([text], 'error'), (v) => (got.push(v), true));
    expect(end).toBe('cut_short');
    expect(got).toEqual(lines);
  });

  it('a line that is not JSON, or refused, cuts the read short', async () => {
    expect(await readNdjson(stream(['{"a":1}\nnot json\n'], 'close'), () => true)).toBe(
      'cut_short',
    );
    expect(await readNdjson(stream(['{"a":1}\n'], 'close'), () => false)).toBe('cut_short');
  });
});

describe('streamLogs', () => {
  it('requests the service with follow and tail, and stdout and stderr stay apart', async () => {
    let url = '';
    const fetchFn = ((u: string) => {
      url = u;
      return Promise.resolve(
        new Response(
          stream([ndjson([line('web', 'stdout', 'out'), line('web', 'stderr', 'err')])], 'close'),
        ),
      );
    }) as typeof fetch;
    const got: LogLineView[] = [];
    const end = await streamLogs(logsPath('my-app', 'web', true, 200), (l) => got.push(l), undefined, fetchFn);
    expect(url).toBe('/api/v1/projects/my-app/logs?service=web&follow=true&tail=200');
    expect(end).toBe('complete');
    expect(got.map((l) => [l.stream, l.line])).toEqual([
      ['stdout', 'out'],
      ['stderr', 'err'],
    ]);
  });

  it('a line of another shape cuts the stream short', async () => {
    const fetchFn = (() =>
      Promise.resolve(new Response(stream(['{"service":"web"}\n'], 'close')))) as typeof fetch;
    expect(await streamLogs('/x', () => {}, undefined, fetchFn)).toBe('cut_short');
  });

  it("an error answer throws the daemon's message", async () => {
    const fetchFn = (() =>
      Promise.resolve(
        new Response(
          JSON.stringify({
            code: 'docker_unavailable',
            message: 'Docker is not reachable',
            details: {},
            operation_id: null,
          }),
          { status: 503 },
        ),
      )) as typeof fetch;
    await expect(streamLogs('/x', () => {}, undefined, fetchFn)).rejects.toThrow(
      new RequestError('Docker is not reachable (docker_unavailable)', 503),
    );
  });
});
