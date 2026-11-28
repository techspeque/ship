// View logic of the project page: status, deployment history and config,
// API values to what the page shows. Pure. State strings are shown exactly
// as the API sends them (ADR-0006: desired state, the last deployment and
// the observed status are three things, each shown as itself).

import type {
  DeploymentList,
  OperationEventView,
  OperationView,
  SecretList,
  ServiceStatus,
  StatusView,
  VarList,
} from './api';
import type { Tone } from './view';

export interface Badge {
  text: string;
  tone: Tone;
  /** A text glyph, so the state never relies on colour. */
  icon: string;
}

const OBSERVED: Record<ServiceStatus, { tone: Tone; icon: string }> = {
  healthy: { tone: 'ok', icon: '✓' },
  running_unverified: { tone: 'neutral', icon: '•' },
  starting: { tone: 'neutral', icon: '…' },
  unhealthy: { tone: 'bad', icon: '✕' },
  failed: { tone: 'bad', icon: '✕' },
  stopped: { tone: 'neutral', icon: '■' },
  paused: { tone: 'warn', icon: '‖' },
  unknown: { tone: 'warn', icon: '?' },
};

/** An observed service status as a badge; its text is the API's string. */
export function observedBadge(status: ServiceStatus): Badge {
  return { text: status, ...OBSERVED[status] };
}

/** A deployment or operation result as a badge; `null` is in progress. */
export function resultBadge(result: string | null): Badge {
  switch (result) {
    case null:
      return { text: 'in progress', tone: 'neutral', icon: '…' };
    case 'succeeded':
      return { text: result, tone: 'ok', icon: '✓' };
    case 'failed':
      return { text: result, tone: 'bad', icon: '✕' };
    default:
      return { text: result, tone: 'warn', icon: '!' };
  }
}

export interface ServiceRow {
  id: string;
  name: string;
  kind: string;
  /** What was asked of it. */
  desired: string;
  /** What Docker reports now. */
  observed: Badge;
  restarts: number;
  observedAt: string;
  staleConfig: boolean;
  endpoints: string[];
}

export interface LastDeploymentSummary {
  action: string;
  result: Badge;
  release: string | null;
  operationId: string;
  startedAt: string;
  endedAt: string | null;
}

export interface StatusPage {
  project: string;
  environment: string;
  /** The environment's desired state. */
  desired: string;
  lastDeployment: LastDeploymentSummary | null;
  services: ServiceRow[];
}

export function statusPage(s: StatusView): StatusPage {
  const d = s.last_deployment;
  return {
    project: s.project.slug,
    environment: s.environment.name,
    desired: s.environment.desired_state,
    lastDeployment:
      d === null
        ? null
        : {
            action: d.action,
            result: resultBadge(d.result),
            release: d.release_id,
            operationId: d.operation_id,
            startedAt: d.started_at,
            endedAt: d.ended_at,
          },
    services: s.services.map((v) => ({
      id: v.id,
      name: v.name,
      kind: v.kind,
      desired: v.desired_state,
      observed: observedBadge(v.status),
      restarts: v.restarts,
      observedAt: v.observed_at,
      staleConfig: v.stale_config,
      endpoints: v.endpoints.map((e) => e.address),
    })),
  };
}

export interface DeploymentRow {
  id: string;
  action: string;
  result: Badge;
  revision: string;
  release: string | null;
  operationId: string;
  startedAt: string;
  endedAt: string | null;
  /** `code in phase: message`, for a failed or interrupted one. */
  failure: { code: string; phase: string; message: string } | null;
}

export function deploymentRows(list: DeploymentList): DeploymentRow[] {
  return list.deployments.map((d) => ({
    id: d.id,
    action: d.action,
    result: resultBadge(d.result),
    revision: d.revision_id,
    release: d.release_id,
    operationId: d.operation_id,
    startedAt: d.started_at,
    endedAt: d.ended_at,
    failure: d.failure === null ? null : { ...d.failure },
  }));
}

export interface ConfigEntry {
  name: string;
  /** A config value; `null` for a secret, whose value is never shown. */
  value: string | null;
  updatedAt: string;
}

export interface ConfigSection {
  /** `Project`, `Environment development`, `Service web`. */
  title: string;
  vars: ConfigEntry[];
  secrets: ConfigEntry[];
}

function title(l: VarList | SecretList): string {
  switch (l.scope) {
    case 'project':
      return 'Project';
    case 'environment':
      return `Environment ${l.environment ?? ''}`.trim();
    case 'service':
      return `Service ${l.service ?? ''}`.trim();
  }
}

/** One scope's values and secret names. */
export function configSection(vars: VarList, secrets: SecretList): ConfigSection {
  return {
    title: title(vars),
    vars: vars.vars.map((v) => ({ name: v.name, value: v.value, updatedAt: v.updated_at })),
    // Built from the name and time only: nothing else of a secret is read.
    secrets: secrets.secrets.map((s) => ({ name: s.name, value: null, updatedAt: s.updated_at })),
  };
}

export interface OperationLine {
  sequence: number;
  phase: string;
  event: string;
  message: string;
  ts: string;
}

export interface OperationPage {
  kind: string;
  phase: string;
  status: Badge;
  failure: { code: string; phase: string; message: string } | null;
  createdAt: string;
}

export function operationPage(op: OperationView): OperationPage {
  return {
    kind: op.kind,
    phase: op.phase,
    status: op.status === 'queued' || op.status === 'running'
      ? { text: op.status, tone: 'neutral', icon: '…' }
      : resultBadge(op.status),
    failure: op.failure === null ? null : { ...op.failure },
    createdAt: op.created_at,
  };
}

export function operationLine(e: OperationEventView): OperationLine {
  return { sequence: e.sequence, phase: e.phase, event: e.event, message: e.message, ts: e.ts };
}
