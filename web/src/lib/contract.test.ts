// The TS half of the type drift guard. The fixtures in ./contract/ are
// written by `crates/ship-daemon/tests/web_contract.rs` from the Rust types
// (`SHIP_UPDATE_CONTRACT=1` rewrites them); the guards in api.ts must
// accept every one, and must reject anything else.

import { describe, expect, it } from 'vitest';

import { DOCKER_STATUSES, isApiError, isHostInfo, isProjectList } from './api';

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
});
