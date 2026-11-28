// The TS half of the type drift guard. The fixtures in ./contract/ are
// written by `crates/ship-daemon/tests/web_contract.rs` from the Rust types
// (`SHIP_UPDATE_CONTRACT=1` rewrites them); the guards in api.ts must
// accept every one, and must reject anything else.

import { describe, expect, it } from 'vitest';

import {
  DEPLOYMENT_ACTIONS,
  DEPLOYMENT_RESULTS,
  DESIRED_STATES,
  DOCKER_STATUSES,
  EVENT_KINDS,
  OPERATION_PHASES,
  OPERATION_STATUSES,
  SERVICE_STATUSES,
  isApiError,
  isDeploymentList,
  isHostInfo,
  isLogLine,
  isOperationEvent,
  isOperationView,
  isProjectList,
  isSecretList,
  isServiceStatus,
  isStatusView,
  isVarList,
} from './api';

const fixtures = import.meta.glob<unknown>('./contract/*.json', {
  eager: true,
  import: 'default',
});

function byPrefix(prefix: string): [string, unknown][] {
  const found = Object.entries(fixtures).filter(([path]) =>
    path.startsWith(`./contract/${prefix}`),
  );
  expect(found.length, `fixtures ${prefix}*`).toBeGreaterThan(0);
  return found;
}

function fixture(name: string): unknown {
  const v = fixtures[`./contract/${name}.json`];
  expect(v, name).toBeDefined();
  return v;
}

/** A deep copy of a fixture, to mutate. */
function copy(name: string): Record<string, unknown> {
  return structuredClone(fixture(name)) as Record<string, unknown>;
}

function at(v: Record<string, unknown>, key: string): Record<string, unknown> {
  return v[key] as Record<string, unknown>;
}

/** The first element of the array at `key`. */
function first(v: Record<string, unknown>, key: string): Record<string, unknown> {
  const item = (v[key] as Record<string, unknown>[])[0];
  if (item === undefined) throw new Error(`fixture has no ${key}`);
  return item;
}

function firstProject(list: Record<string, unknown>): Record<string, unknown> {
  const first = (list['projects'] as Record<string, unknown>[])[0];
  if (first === undefined) throw new Error('fixture has no project');
  return first;
}

describe('fixtures from the Rust types', () => {
  it('every host fixture passes isHostInfo', () => {
    for (const [path, v] of byPrefix('host-')) expect(isHostInfo(v), path).toBe(true);
  });

  it('every projects fixture passes isProjectList', () => {
    for (const [path, v] of byPrefix('projects-')) expect(isProjectList(v), path).toBe(true);
  });

  it('every error fixture passes isApiError', () => {
    for (const [path, v] of byPrefix('error-')) expect(isApiError(v), path).toBe(true);
  });

  it('the TS DockerStatus list equals the Rust one, in order', () => {
    expect([...DOCKER_STATUSES]).toEqual(fixture('docker-statuses'));
  });

  it('the TS ServiceStatus list equals the Rust one, in order', () => {
    expect([...SERVICE_STATUSES]).toEqual(fixture('service-statuses'));
  });

  it('isServiceStatus accepts every Rust service status, paused included', () => {
    const statuses = fixture('service-statuses') as unknown[];
    expect(statuses).toContain('paused');
    for (const s of statuses) expect(isServiceStatus(s), String(s)).toBe(true);
  });

  it('every status fixture passes isStatusView', () => {
    for (const [path, v] of byPrefix('status-')) expect(isStatusView(v), path).toBe(true);
  });

  it('the deployments fixture passes isDeploymentList', () => {
    expect(isDeploymentList(fixture('deployments'))).toBe(true);
  });

  it('every operation fixture passes isOperationView', () => {
    for (const [path, v] of byPrefix('operation-running').concat(byPrefix('operation-failed'))) {
      expect(isOperationView(v), path).toBe(true);
    }
  });

  it('the event and log line fixtures pass their guards', () => {
    expect(isOperationEvent(fixture('operation-event'))).toBe(true);
    expect(isLogLine(fixture('log-line'))).toBe(true);
  });

  it('every vars and secrets fixture passes its guard', () => {
    for (const [path, v] of byPrefix('vars-')) expect(isVarList(v), path).toBe(true);
    for (const [path, v] of byPrefix('secrets-')) expect(isSecretList(v), path).toBe(true);
  });

  it('the TS enum lists equal the Rust ones, in order', () => {
    expect([...OPERATION_PHASES]).toEqual(fixture('operation-phases'));
    expect([...OPERATION_STATUSES]).toEqual(fixture('operation-statuses'));
    expect([...DESIRED_STATES]).toEqual(fixture('desired-states'));
    expect([...DEPLOYMENT_RESULTS]).toEqual(fixture('deployment-results'));
    expect([...DEPLOYMENT_ACTIONS]).toEqual(fixture('deployment-actions'));
    expect([...EVENT_KINDS]).toEqual(fixture('event-kinds'));
  });

  it('there is a host fixture for every Docker status', () => {
    const seen = byPrefix('host-').map(
      ([, v]) => at(v as Record<string, unknown>, 'docker')['status'],
    );
    expect(new Set(seen)).toEqual(new Set(DOCKER_STATUSES));
  });
});

describe('guards reject other shapes', () => {
  it('rejects an extra key', () => {
    const h = copy('host-ready');
    h['extra'] = 1;
    expect(isHostInfo(h)).toBe(false);
    const nested = copy('host-ready');
    at(nested, 'capacity')['gpus'] = 1;
    expect(isHostInfo(nested)).toBe(false);
    const p = copy('projects-one');
    firstProject(p)['name'] = 'x';
    expect(isProjectList(p)).toBe(false);
  });

  it('rejects a missing key', () => {
    const h = copy('host-ready');
    delete h['os'];
    expect(isHostInfo(h)).toBe(false);
    const nested = copy('host-ready');
    delete at(nested, 'docker')['message'];
    expect(isHostInfo(nested)).toBe(false);
    const p = copy('projects-one');
    delete firstProject(p)['slug'];
    expect(isProjectList(p)).toBe(false);
    expect(isProjectList({})).toBe(false);
  });

  it('rejects a wrong type', () => {
    const h = copy('host-ready');
    at(h, 'capacity')['cpus'] = '4';
    expect(isHostInfo(h)).toBe(false);
    const neg = copy('host-ready');
    at(neg, 'capacity')['cpus'] = -1;
    expect(isHostInfo(neg)).toBe(false);
    const v = copy('host-ready');
    v['version'] = null;
    expect(isHostInfo(v)).toBe(false);
    expect(isProjectList({ projects: {} })).toBe(false);
    expect(isHostInfo(null)).toBe(false);
    expect(isHostInfo([])).toBe(false);
  });

  it('rejects an unknown status', () => {
    const h = copy('host-ready');
    at(h, 'docker')['status'] = 'stopped';
    expect(isHostInfo(h)).toBe(false);
  });

  it('rejects changed status, deployment, operation and config shapes', () => {
    const cases: [string, (v: unknown) => boolean, (v: Record<string, unknown>) => void][] = [
      ['status-deployed', isStatusView, (v) => (v['extra'] = 1)],
      ['status-deployed', isStatusView, (v) => delete v['last_deployment']],
      ['status-deployed', isStatusView, (v) => (at(v, 'environment')['desired_state'] = 'gone')],
      ['status-deployed', isStatusView, (v) => (first(v, 'services')['status'] = 'up')],
      ['status-deployed', isStatusView, (v) => (first(v, 'services')['restarts'] = -1)],
      ['deployments', isDeploymentList, (v) => (v['deployments'] = {})],
      ['deployments', isDeploymentList, (v) => (first(v, 'deployments')['result'] = 'done')],
      ['operation-running', isOperationView, (v) => (v['phase'] = 'sleeping')],
      ['operation-running', isOperationView, (v) => delete v['failure']],
      ['operation-event', isOperationEvent, (v) => (v['sequence'] = 0)],
      ['operation-event', isOperationEvent, (v) => (v['sequence'] = '3')],
      ['log-line', isLogLine, (v) => (v['extra'] = '')],
      ['vars-project', isVarList, (v) => (v['scope'] = 'host')],
      ['secrets-environment', isSecretList, (v) => (v['secrets'] = [{ name: 'X' }])],
    ];
    for (const [name, guard, change] of cases) {
      const v = copy(name);
      change(v);
      expect(guard(v), `${name}: ${guard.name}`).toBe(false);
    }
  });

  it('rejects a secret list that carries a value', () => {
    const v = copy('secrets-environment');
    first(v, 'secrets')['value'] = 'hunter2';
    expect(isSecretList(v)).toBe(false);
  });

  it('rejects an unknown service status', () => {
    for (const v of ['suspended', 'Paused', 'ready', '', null, 7, ['paused']]) {
      expect(isServiceStatus(v), JSON.stringify(v)).toBe(false);
    }
  });
});
