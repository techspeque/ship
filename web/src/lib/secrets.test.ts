// No secret value is present in any page or request the UI makes (plan
// §1.13). The UI has no request that writes a secret; every request it can
// make is sent here against a recording `fetch`, and none carries a body or
// the test secret. A secret list carrying a value is refused, so it never
// reaches the page.

import { describe, expect, it } from 'vitest';

import { ACTIONS, submitAction, type Target } from './actions';
import { deploymentsPath, isDeploymentList, isStatusView, logsPath, statusPath } from './api';
import deployments from './contract/deployments.json';
import event from './contract/operation-event.json';
import logLine from './contract/log-line.json';
import opRunning from './contract/operation-running.json';
import secretsEnv from './contract/secrets-environment.json';
import statusDeployed from './contract/status-deployed.json';
import varsProject from './contract/vars-project.json';
import { load, loadConfig } from './load';
import { streamLogs } from './ndjson';
import { followOperation } from './sse';

const SECRET = 'test-secret-7f3a9c';

function answer(url: string, leak: boolean): Response {
  const path = url.split('?')[0] ?? url;
  const json = (v: unknown, status = 200) => new Response(JSON.stringify(v), { status });
  if (path.endsWith('/status')) return json(statusDeployed);
  if (path.endsWith('/deployments')) return json(deployments);
  if (path.endsWith('/vars')) return json(varsProject);
  if (path.endsWith('/secrets')) {
    if (!leak) return json(secretsEnv);
    const leaked = structuredClone(secretsEnv) as { secrets: Record<string, unknown>[] };
    for (const s of leaked.secrets) s['value'] = SECRET;
    return json(leaked);
  }
  if (path.endsWith('/logs')) return new Response(JSON.stringify(logLine) + '\n');
  if (path.endsWith('/events')) {
    const finished = { ...event, sequence: 1, event: 'finished', phase: 'succeeded' };
    return new Response(`id: 1\ndata: ${JSON.stringify(finished)}\n\n`);
  }
  return json(opRunning, 202);
}

function recorder(leak = false) {
  const sent: { url: string; init: RequestInit | undefined }[] = [];
  const fetchFn = ((url: string, init?: RequestInit) => {
    sent.push({ url, init });
    return Promise.resolve(answer(url, leak));
  }) as typeof fetch;
  return { sent, fetchFn };
}

describe('the requests the UI makes', () => {
  it('carry no body and no secret value', async () => {
    const { sent, fetchFn } = recorder();
    await load(statusPath('my-app'), isStatusView, fetchFn);
    await load(deploymentsPath('my-app'), isDeploymentList, fetchFn);
    const config = await loadConfig('my-app', 'development', ['web', 'worker'], fetchFn);
    expect(config.kind).toBe('ok');
    await streamLogs(logsPath('my-app', null, true, 200), () => {}, undefined, fetchFn);
    await followOperation(opRunning.id, () => {}, { fetchFn, sleep: () => Promise.resolve() });
    const target: Target = {
      project: 'my-app',
      environment: 'development',
      service: null,
      services: ['web'],
    };
    for (const a of ACTIONS) await submitAction(a, target, true, fetchFn);

    // 2 + 2×(project, environment, 2 services) + logs + events + 5 actions.
    expect(sent).toHaveLength(2 + 8 + 1 + 1 + ACTIONS.length);
    for (const { url, init } of sent) {
      expect(init?.body, url).toBeUndefined();
      expect(JSON.stringify({ url, headers: init?.headers })).not.toContain(SECRET);
      expect(['GET', 'POST']).toContain(init?.method);
      if (init?.method === 'POST') expect(url).toMatch(/\/(start|stop|restart|redeploy|remove)\?/);
    }
    // No request reaches a route that writes a secret or a config value.
    expect(sent.filter((s) => s.init?.method === 'POST' && /secrets|vars/.test(s.url))).toEqual(
      [],
    );
  });

  it('a secret list carrying a value is refused, so it never reaches the page', async () => {
    const { fetchFn } = recorder(true);
    const config = await loadConfig('my-app', 'development', ['web'], fetchFn);
    expect(config.kind).toBe('error');
    expect(JSON.stringify(config)).not.toContain(SECRET);
  });
});
