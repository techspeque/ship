// The project page shows exactly what the API returned: desired state,
// last deployment and observed status as three separate values, each the
// API's own string (plan §1.13: "every state the UI shows equals what the
// API returns").

import { describe, expect, it } from 'vitest';

import {
  SERVICE_STATUSES,
  isDeploymentList,
  isOperationView,
  isSecretList,
  isStatusView,
  isVarList,
  type DeploymentList,
  type OperationView,
  type SecretList,
  type StatusView,
  type VarList,
} from './api';
import deploymentsJson from './contract/deployments.json';
import opFailedJson from './contract/operation-failed.json';
import opRunningJson from './contract/operation-running.json';
import secretsEnvJson from './contract/secrets-environment.json';
import statusDeployedJson from './contract/status-deployed.json';
import statusNeverJson from './contract/status-never-deployed.json';
import varsProjectJson from './contract/vars-project.json';
import varsServiceJson from './contract/vars-service.json';
import {
  configSection,
  deploymentRows,
  observedBadge,
  operationPage,
  statusPage,
} from './project';

function guarded<T>(v: unknown, guard: (v: unknown) => v is T): T {
  if (!guard(v)) throw new Error('fixture does not pass its guard');
  return v;
}

const deployed = guarded<StatusView>(statusDeployedJson, isStatusView);
const never = guarded<StatusView>(statusNeverJson, isStatusView);
const history = guarded<DeploymentList>(deploymentsJson, isDeploymentList);

describe('statusPage', () => {
  it('shows each service desired and observed apart, equal to the API', () => {
    const page = statusPage(deployed);
    expect(page.project).toBe(deployed.project.slug);
    expect(page.environment).toBe(deployed.environment.name);
    expect(page.desired).toBe(deployed.environment.desired_state);
    expect(
      page.services.map((s) => [s.name, s.kind, s.desired, s.observed.text, s.restarts]),
    ).toEqual(
      deployed.services.map((s) => [s.name, s.kind, s.desired_state, s.status, s.restarts]),
    );
    expect(page.services.map((s) => s.staleConfig)).toEqual(
      deployed.services.map((s) => s.stale_config),
    );
    expect(page.services.map((s) => s.endpoints)).toEqual(
      deployed.services.map((s) => s.endpoints.map((e) => e.address)),
    );
    expect(page.services.map((s) => s.observedAt)).toEqual(
      deployed.services.map((s) => s.observed_at),
    );
  });

  it('a service stopped on purpose reads desired stopped, observed stopped', () => {
    const worker = statusPage(deployed).services.find((s) => s.name === 'worker');
    expect(worker?.desired).toBe('stopped');
    expect(worker?.observed.text).toBe('stopped');
  });

  it('shows the last deployment as the API reports it', () => {
    const d = deployed.last_deployment;
    const shown = statusPage(deployed).lastDeployment;
    expect(d).not.toBeNull();
    expect(shown).toEqual({
      action: d?.action,
      result: { text: d?.result, tone: 'ok', icon: '✓' },
      release: d?.release_id,
      operationId: d?.operation_id,
      startedAt: d?.started_at,
      endedAt: d?.ended_at,
    });
  });

  it('a never-deployed environment has no last deployment and no services', () => {
    const page = statusPage(never);
    expect(page.lastDeployment).toBeNull();
    expect(page.services).toEqual([]);
  });

  it('every service status is shown as its own string with a glyph', () => {
    for (const s of SERVICE_STATUSES) {
      const b = observedBadge(s);
      expect(b.text).toBe(s);
      expect(b.icon).not.toBe('');
    }
    // `healthy` is the only status shown as good (ADR-0006).
    expect(SERVICE_STATUSES.filter((s) => observedBadge(s).tone === 'ok')).toEqual(['healthy']);
  });
});

describe('deploymentRows', () => {
  it('lists every deployment in order, with its result as the API says it', () => {
    const rows = deploymentRows(history);
    expect(rows.map((r) => [r.id, r.action, r.operationId, r.startedAt, r.endedAt])).toEqual(
      history.deployments.map((d) => [d.id, d.action, d.operation_id, d.started_at, d.ended_at]),
    );
    expect(rows.map((r) => r.result.text)).toEqual(
      history.deployments.map((d) => d.result ?? 'in progress'),
    );
  });

  it('a failed deployment shows its code, phase and message', () => {
    const rows = deploymentRows(history);
    const failed = history.deployments.filter((d) => d.failure !== null);
    expect(failed.length).toBeGreaterThan(0);
    for (const d of failed) {
      expect(rows.find((r) => r.id === d.id)?.failure).toEqual(d.failure);
    }
    expect(rows.filter((r) => r.failure === null).length).toBe(
      history.deployments.length - failed.length,
    );
  });
});

describe('configSection', () => {
  const varsProject = guarded<VarList>(varsProjectJson, isVarList);
  const varsService = guarded<VarList>(varsServiceJson, isVarList);
  const secrets = guarded<SecretList>(secretsEnvJson, isSecretList);

  it('shows values and secret names with their times, and no secret value', () => {
    const s = configSection(varsProject, secrets);
    expect(s.title).toBe('Project');
    expect(s.vars).toEqual(
      varsProject.vars.map((v) => ({ name: v.name, value: v.value, updatedAt: v.updated_at })),
    );
    expect(s.secrets).toEqual(
      secrets.secrets.map((v) => ({ name: v.name, value: null, updatedAt: v.updated_at })),
    );
  });

  it('names the scope', () => {
    expect(configSection(varsService, secrets).title).toBe('Service web');
    expect(configSection({ ...varsService, scope: 'environment' }, secrets).title).toBe(
      'Environment development',
    );
  });
});

describe('operationPage', () => {
  it('shows a running operation as running and a failed one with its phase', () => {
    const running = guarded<OperationView>(opRunningJson, isOperationView);
    const failed = guarded<OperationView>(opFailedJson, isOperationView);
    expect(operationPage(running)).toMatchObject({
      kind: running.kind,
      phase: running.phase,
      status: { text: 'running' },
      failure: null,
    });
    expect(operationPage(failed)).toMatchObject({
      phase: 'failed',
      status: { text: 'failed', tone: 'bad' },
      failure: failed.failure,
    });
  });
});
