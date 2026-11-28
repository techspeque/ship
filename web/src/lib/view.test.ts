import { describe, expect, it } from 'vitest';

import { DOCKER_STATUSES, type DockerStatus, type HostInfo, type ProjectList } from './api';
import projectsOne from './contract/projects-one.json';
import {
  UNKNOWN,
  announcement,
  formatBytes,
  formatCpus,
  hostView,
  projectsView,
  type HostView,
} from './view';

function host(status: DockerStatus, message: string | null = null): HostInfo {
  return {
    api_version: 'v1',
    version: '0.0.1',
    os: 'linux',
    arch: 'x86_64',
    docker: {
      status,
      engine_version: status === 'unreachable' ? null : '28.3.2',
      compose_version: status === 'unreachable' ? null : '2.39.1',
      supported: { engine: '>= 24.0.0, < 30.0.0', compose: '>= 2.20.0, < 6.0.0' },
      message,
    },
    capacity: { cpus: 4, memory_bytes: 8 * 1024 ** 3, disk_free_bytes: null },
  };
}

function detail(view: HostView, term: string): string | undefined {
  return view.details.find((d) => d.term === term)?.value;
}

describe('hostView', () => {
  it('gives each Docker state its own label, tone and icon', () => {
    const views = DOCKER_STATUSES.map((s) => hostView({ kind: 'ok', value: host(s) }));
    for (const key of ['label', 'tone', 'icon'] as const) {
      expect(new Set(views.map((v) => v[key])).size).toBe(DOCKER_STATUSES.length);
    }
    const [ready, unreachable, unsupported] = views;
    expect(ready).toMatchObject({ label: 'Docker ready', tone: 'ok' });
    expect(unreachable).toMatchObject({ label: 'Docker unreachable', tone: 'bad' });
    expect(unsupported).toMatchObject({ label: 'Docker version unsupported', tone: 'warn' });
  });

  it('shows versions, supported ranges, capacity and the daemon message', () => {
    const v = hostView({
      kind: 'ok',
      value: host('unsupported_version', 'Docker Engine 23.0.1 is older than 24.0.0'),
    });
    expect(v.message).toBe('Docker Engine 23.0.1 is older than 24.0.0');
    expect(detail(v, 'Ship')).toBe('0.0.1 (API v1)');
    expect(detail(v, 'Platform')).toBe('linux/x86_64');
    expect(detail(v, 'Docker Engine')).toBe('28.3.2');
    expect(detail(v, 'Docker Compose')).toBe('2.39.1');
    expect(detail(v, 'Supported Engine')).toBe('>= 24.0.0, < 30.0.0');
    expect(detail(v, 'Supported Compose')).toBe('>= 2.20.0, < 6.0.0');
    expect(detail(v, 'CPUs')).toBe('4 CPUs');
    expect(detail(v, 'Memory')).toBe('8.0 GiB');
    expect(detail(v, 'Disk free')).toBe(UNKNOWN);
  });

  it('shows unknown versions as unknown when Docker is unreachable', () => {
    const v = hostView({ kind: 'ok', value: host('unreachable', 'cannot connect') });
    expect(detail(v, 'Docker Engine')).toBe(UNKNOWN);
    expect(detail(v, 'Docker Compose')).toBe(UNKNOWN);
    expect(v.message).toBe('cannot connect');
  });

  it('has a loading state', () => {
    const v = hostView({ kind: 'loading' });
    expect(v.tone).toBe('neutral');
    expect(v.label).toMatch(/checking/i);
    expect(v.details).toEqual([]);
    expect(v.message).toBeNull();
  });

  it('has an error state carrying the message', () => {
    const v = hostView({ kind: 'error', message: 'no such endpoint (not_found)' });
    expect(v.tone).toBe('bad');
    expect(v.label).toBe('Cannot read host status');
    expect(v.message).toBe('no such endpoint (not_found)');
    expect(v.details).toEqual([]);
    const ready = hostView({ kind: 'ok', value: host('ready') });
    expect(v.label).not.toBe(ready.label);
  });
});

describe('projectsView', () => {
  it('has loading, error and empty states', () => {
    expect(projectsView({ kind: 'loading' }).kind).toBe('loading');
    expect(projectsView({ kind: 'error', message: 'boom' })).toEqual({
      kind: 'error',
      text: 'boom',
    });
    expect(projectsView({ kind: 'ok', value: { projects: [] } })).toEqual({
      kind: 'empty',
      text: 'No projects yet.',
    });
  });

  it('lists projects', () => {
    const v = projectsView({
      kind: 'ok',
      value: {
        projects: [
          {
            id: 'prj_01aryz6s41tsv4rrffq69g5fav',
            slug: 'my-app',
            created_at: '2016-07-30T22:36:16.385Z',
            environments: [],
          },
        ],
      },
    });
    expect(v).toEqual({
      kind: 'list',
      rows: [
        {
          id: 'prj_01aryz6s41tsv4rrffq69g5fav',
          slug: 'my-app',
          created: '2016-07-30T22:36:16.385Z',
          environments: [],
        },
      ],
    });
  });

  it("shows each environment's desired state and observed status as the API sends them", () => {
    const list = projectsOne as ProjectList;
    const v = projectsView({ kind: 'ok', value: list });
    if (v.kind !== 'list') throw new Error(v.kind);
    const api = list.projects.flatMap((p) => p.environments);
    const shown = v.rows.flatMap((r) => r.environments);
    expect(shown.map((e) => [e.name, e.desired, e.status])).toEqual(
      api.map((e) => [e.name, e.desired_state, e.status]),
    );
  });
});

describe('announcement', () => {
  it('names the host state and the projects', () => {
    const h = hostView({ kind: 'ok', value: host('ready') });
    expect(announcement(h, projectsView({ kind: 'ok', value: { projects: [] } }))).toBe(
      'Docker ready. No projects yet.',
    );
  });
});

describe('formatting', () => {
  it('formats bytes in binary units', () => {
    expect(formatBytes(null)).toBe(UNKNOWN);
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(1023)).toBe('1023 B');
    expect(formatBytes(1024)).toBe('1.0 KiB');
    expect(formatBytes(1536)).toBe('1.5 KiB');
    expect(formatBytes(8_589_934_592)).toBe('8.0 GiB');
    expect(formatBytes(5 * 1024 ** 4)).toBe('5.0 TiB');
  });

  it('formats CPUs', () => {
    expect(formatCpus(null)).toBe(UNKNOWN);
    expect(formatCpus(1)).toBe('1 CPU');
    expect(formatCpus(16)).toBe('16 CPUs');
  });
});
