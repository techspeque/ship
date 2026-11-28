// View logic: API values to what the page shows. Pure; components only
// render what these functions return.

import type { DockerStatus, HostInfo, ProjectList } from './api';

/** A request's state as the page tracks it. */
export type Load<T> =
  | { kind: 'loading' }
  | { kind: 'error'; message: string }
  | { kind: 'ok'; value: T };

/** Colour family of a status; each Docker state has its own. */
export type Tone = 'neutral' | 'ok' | 'warn' | 'bad';

export interface Detail {
  term: string;
  value: string;
}

export interface HostView {
  /** Short status text, also announced through the live region. */
  label: string;
  tone: Tone;
  /** A text glyph beside the label, so the state never relies on colour. */
  icon: string;
  /** The daemon's or the request's message, when there is one. */
  message: string | null;
  details: Detail[];
}

export const UNKNOWN = 'unknown';

const DOCKER: Record<DockerStatus, { label: string; tone: Tone; icon: string }> = {
  ready: { label: 'Docker ready', tone: 'ok', icon: '✓' },
  unreachable: { label: 'Docker unreachable', tone: 'bad', icon: '✕' },
  unsupported_version: { label: 'Docker version unsupported', tone: 'warn', icon: '!' },
};

/** Bytes in binary units, one decimal from KiB up; `null` is unknown. */
export function formatBytes(bytes: number | null): string {
  if (bytes === null) return UNKNOWN;
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB', 'EiB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${value} B` : `${value.toFixed(1)} ${units[unit]}`;
}

/** `1 CPU`, `4 CPUs`; `null` is unknown. */
export function formatCpus(cpus: number | null): string {
  if (cpus === null) return UNKNOWN;
  return cpus === 1 ? '1 CPU' : `${cpus} CPUs`;
}

function orUnknown(v: string | null): string {
  return v ?? UNKNOWN;
}

export function hostView(load: Load<HostInfo>): HostView {
  switch (load.kind) {
    case 'loading':
      return { label: 'Checking the host…', tone: 'neutral', icon: '…', message: null, details: [] };
    case 'error':
      return {
        label: 'Cannot read host status',
        tone: 'bad',
        icon: '✕',
        message: load.message,
        details: [],
      };
    case 'ok': {
      const h = load.value;
      const d = DOCKER[h.docker.status];
      return {
        label: d.label,
        tone: d.tone,
        icon: d.icon,
        message: h.docker.message,
        details: [
          { term: 'Ship', value: `${h.version} (API ${h.api_version})` },
          { term: 'Platform', value: `${h.os}/${h.arch}` },
          { term: 'Docker Engine', value: orUnknown(h.docker.engine_version) },
          { term: 'Docker Compose', value: orUnknown(h.docker.compose_version) },
          { term: 'Supported Engine', value: h.docker.supported.engine },
          { term: 'Supported Compose', value: h.docker.supported.compose },
          { term: 'CPUs', value: formatCpus(h.capacity.cpus) },
          { term: 'Memory', value: formatBytes(h.capacity.memory_bytes) },
          { term: 'Disk free', value: formatBytes(h.capacity.disk_free_bytes) },
        ],
      };
    }
  }
}

export interface EnvironmentRow {
  id: string;
  name: string;
  /** What the operator asked for, as the API says it. */
  desired: string;
  /** The observed overall status, as the API says it; `null` with no service. */
  status: string | null;
}

export interface ProjectRow {
  id: string;
  slug: string;
  created: string;
  environments: EnvironmentRow[];
}

export type ProjectsView =
  | { kind: 'loading'; text: string }
  | { kind: 'error'; text: string }
  | { kind: 'empty'; text: string }
  | { kind: 'list'; rows: ProjectRow[] };

export function projectsView(load: Load<ProjectList>): ProjectsView {
  switch (load.kind) {
    case 'loading':
      return { kind: 'loading', text: 'Loading projects…' };
    case 'error':
      return { kind: 'error', text: load.message };
    case 'ok':
      if (load.value.projects.length === 0) return { kind: 'empty', text: 'No projects yet.' };
      return {
        kind: 'list',
        rows: load.value.projects.map((p) => ({
          id: p.id,
          slug: p.slug,
          created: p.created_at,
          environments: p.environments.map((e) => ({
            id: e.id,
            name: e.name,
            desired: e.desired_state,
            status: e.status,
          })),
        })),
      };
  }
}

/** What the live region says after a refresh: the host state, then projects. */
export function announcement(host: HostView, projects: ProjectsView): string {
  const p =
    projects.kind === 'list'
      ? `${projects.rows.length} ${projects.rows.length === 1 ? 'project' : 'projects'}`
      : projects.text;
  return `${host.label}. ${p}`;
}
