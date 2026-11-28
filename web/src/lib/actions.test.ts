import { describe, expect, it } from 'vitest';

import opRunning from './contract/operation-running.json';
import {
  ACTIONS,
  ActionRefused,
  actionPath,
  actionsFor,
  confirmation,
  needsConfirmation,
  submitAction,
  type Target,
} from './actions';

const project: Target = {
  project: 'my-app',
  environment: 'development',
  service: null,
  services: ['web', 'worker'],
};
const web: Target = { ...project, service: 'web' };

interface Sent {
  url: string;
  init: RequestInit | undefined;
}

function recorder(status = 202, answer: unknown = opRunning) {
  const sent: Sent[] = [];
  const fetchFn = ((url: string, init?: RequestInit) => {
    sent.push({ url, init });
    return Promise.resolve(new Response(JSON.stringify(answer), { status }));
  }) as typeof fetch;
  return { sent, fetchFn };
}

describe('which actions apply', () => {
  it('a project has all five; a service only start, stop and restart', () => {
    expect(actionsFor(project)).toEqual(['start', 'stop', 'restart', 'redeploy', 'remove']);
    expect(actionsFor(web)).toEqual(['start', 'stop', 'restart']);
  });

  it('every action but start needs a confirmation', () => {
    expect(ACTIONS.filter(needsConfirmation)).toEqual(['stop', 'restart', 'redeploy', 'remove']);
  });
});

describe('the confirmation names what is affected', () => {
  it('names the project, the environment and its services', () => {
    for (const a of ACTIONS) {
      const c = confirmation(a, project);
      expect(c.title).toContain('my-app');
      expect(c.title).toContain('development');
      expect(c.lines.join(' ')).toContain('services web, worker');
    }
  });

  it('names the one service for a service action', () => {
    for (const a of actionsFor(web)) {
      expect(confirmation(a, web).lines.join(' ')).toContain('service web');
      expect(confirmation(a, web).lines.join(' ')).not.toContain('worker');
    }
  });

  it('remove says volumes and history are kept', () => {
    const text = confirmation('remove', project).lines.join(' ');
    expect(text).toContain('containers and networks');
    expect(text).toContain('Named volumes');
    expect(text).toContain('history are kept');
  });
});

describe('submitAction', () => {
  it('sends nothing for an action that needs a confirmation it did not get', async () => {
    for (const a of ACTIONS.filter(needsConfirmation)) {
      const { sent, fetchFn } = recorder();
      await expect(submitAction(a, project, false, fetchFn)).rejects.toBeInstanceOf(ActionRefused);
      expect(sent, a).toEqual([]);
    }
  });

  it('sends nothing for redeploy or remove of one service', async () => {
    for (const a of ['redeploy', 'remove'] as const) {
      const { sent, fetchFn } = recorder();
      await expect(submitAction(a, web, true, fetchFn)).rejects.toBeInstanceOf(ActionRefused);
      expect(sent).toEqual([]);
    }
  });

  it('sends a confirmed action as a POST with a key and no body, answered with its operation', async () => {
    const { sent, fetchFn } = recorder();
    const op = await submitAction('stop', web, true, fetchFn);
    expect(op).toEqual(opRunning);
    expect(sent).toHaveLength(1);
    const [{ url, init }] = sent as [Sent];
    expect(url).toBe('/api/v1/projects/my-app/stop?env=development&service=web');
    expect(init?.method).toBe('POST');
    expect(init?.body).toBeUndefined();
    expect(init?.credentials).toBe('same-origin');
    const h = new Headers(init?.headers);
    expect(h.get('idempotency-key')).toMatch(/^[\x21-\x7e]{1,128}$/);
    expect(h.get('ship-api-version')).toBe('v1');
  });

  it('start needs no confirmation', async () => {
    const { sent, fetchFn } = recorder();
    await submitAction('start', project, false, fetchFn);
    expect(sent.map((s) => s.url)).toEqual(['/api/v1/projects/my-app/start?env=development']);
  });

  it('each submission has its own idempotency key', async () => {
    const { sent, fetchFn } = recorder();
    await submitAction('restart', project, true, fetchFn);
    await submitAction('restart', project, true, fetchFn);
    const keys = sent.map((s) => new Headers(s.init?.headers).get('idempotency-key'));
    expect(new Set(keys).size).toBe(2);
  });

  it("an error answer is thrown with the daemon's message", async () => {
    const { fetchFn } = recorder(409, {
      code: 'conflict',
      message: 'the environment was removed; push with --recreate',
      details: {},
      operation_id: null,
    });
    await expect(submitAction('start', project, false, fetchFn)).rejects.toThrow(
      'the environment was removed; push with --recreate (conflict)',
    );
  });

  it('paths escape the project and service', () => {
    expect(actionPath('stop', { ...web, project: 'a', service: 'w b' })).toBe(
      '/api/v1/projects/a/stop?env=development&service=w%20b',
    );
  });
});
