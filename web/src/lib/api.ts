// Wire types of the daemon's API, written by hand to match
// `ship-core::api` and `ship-core::error`, with exact-shape runtime guards.
//
// Drift guard: `crates/ship-daemon/tests/web_contract.rs` serialises the
// Rust types into `./contract/*.json`, and `contract.test.ts` runs these
// guards over those fixtures. The UI runs the same guards on every
// response, so a shape it does not know is shown as an error, never
// rendered as something else.

/** The API version this UI speaks; sent as `Ship-Api-Version`. */
export const API_VERSION = 'v1';
export const API_VERSION_HEADER = 'Ship-Api-Version';
export const HOST_PATH = '/api/v1/host';
export const PROJECTS_PATH = '/api/v1/projects';
export const IDEMPOTENCY_KEY_HEADER = 'Idempotency-Key';
export const LAST_EVENT_ID_HEADER = 'Last-Event-ID';

/** `ship_core::api::DockerStatus`, in declaration order. */
export const DOCKER_STATUSES = ['ready', 'unreachable', 'unsupported_version'] as const;
export type DockerStatus = (typeof DOCKER_STATUSES)[number];

/** `ship_core::ServiceStatus` (ADR-0006 rule 2), in declaration order. */
export const SERVICE_STATUSES = [
  'healthy',
  'running_unverified',
  'starting',
  'unhealthy',
  'failed',
  'stopped',
  'paused',
  'unknown',
] as const;
export type ServiceStatus = (typeof SERVICE_STATUSES)[number];

export interface SupportedVersions {
  engine: string;
  compose: string;
}

export interface DockerInfo {
  status: DockerStatus;
  engine_version: string | null;
  compose_version: string | null;
  supported: SupportedVersions;
  message: string | null;
}

export interface HostCapacity {
  cpus: number | null;
  memory_bytes: number | null;
  disk_free_bytes: number | null;
}

export interface HostInfo {
  api_version: string;
  version: string;
  os: string;
  arch: string;
  docker: DockerInfo;
  capacity: HostCapacity;
}

/** One environment of a project, `ship_core::api::EnvironmentSummary`. */
export interface EnvironmentSummary {
  id: string;
  name: string;
  /** What the operator asked for: `running`, `stopped` or `removed`. */
  desired_state: string;
  /** The overall status of its services now; `null` with no service. */
  status: ServiceStatus | null;
}

export interface ProjectSummary {
  id: string;
  slug: string;
  /** RFC 3339, UTC, millisecond precision. */
  created_at: string;
  environments: EnvironmentSummary[];
}

export interface ProjectList {
  projects: ProjectSummary[];
}

/** `ship_core::DesiredState`, in declaration order. */
export const DESIRED_STATES = ['running', 'stopped', 'removed'] as const;
export type DesiredState = (typeof DESIRED_STATES)[number];

/** `ship_core::OperationPhase`, in declaration order. */
export const OPERATION_PHASES = [
  'queued',
  'uploading',
  'validating',
  'building',
  'preparing',
  'deploying',
  'verifying',
  'succeeded',
  'failed',
  'interrupted',
] as const;
export type OperationPhase = (typeof OPERATION_PHASES)[number];

/** `ship_core::OperationStatus`, in declaration order. */
export const OPERATION_STATUSES = [
  'queued',
  'running',
  'succeeded',
  'failed',
  'interrupted',
] as const;
export type OperationStatus = (typeof OPERATION_STATUSES)[number];

/** `ship_core::DeploymentResult`, in declaration order. */
export const DEPLOYMENT_RESULTS = ['succeeded', 'failed', 'interrupted'] as const;
export type DeploymentResult = (typeof DEPLOYMENT_RESULTS)[number];

/** `ship_core::DeploymentAction`, in declaration order. */
export const DEPLOYMENT_ACTIONS = ['deploy', 'remove'] as const;
export type DeploymentAction = (typeof DEPLOYMENT_ACTIONS)[number];

/**
 * `ship_core::api::EventKind`, in declaration order. An event's `event` is
 * open (readers tolerate new kinds), so it is typed as a string.
 */
export const EVENT_KINDS = ['phase_started', 'log', 'waiting', 'finished'] as const;

/** `ship_core::ConfigScope`. */
export const CONFIG_SCOPES = ['project', 'environment', 'service'] as const;
export type ConfigScope = (typeof CONFIG_SCOPES)[number];

/** `ship_core::api::ProjectRef`. */
export interface ProjectRef {
  id: string;
  slug: string;
}

/** `ship_core::api::EnvironmentView`. */
export interface EnvironmentView {
  id: string;
  name: string;
  desired_state: DesiredState;
}

/** `ship_core::api::LastDeploymentView`. */
export interface LastDeploymentView {
  id: string;
  action: DeploymentAction;
  release_id: string | null;
  operation_id: string;
  /** `null` while in progress. */
  result: DeploymentResult | null;
  started_at: string;
  ended_at: string | null;
}

/** `ship_core::api::EndpointView`. */
export interface EndpointView {
  provider: string;
  address: string;
}

/** `ship_core::api::ServiceStatusView`. */
export interface ServiceStatusView {
  id: string;
  name: string;
  kind: string;
  desired_state: DesiredState;
  status: ServiceStatus;
  restarts: number;
  observed_at: string;
  stale_config: boolean;
  endpoints: EndpointView[];
}

/** `GET /api/v1/projects/{p}/status`: `ship_core::api::StatusView`. */
export interface StatusView {
  project: ProjectRef;
  environment: EnvironmentView;
  last_deployment: LastDeploymentView | null;
  services: ServiceStatusView[];
}

/** `ship_core::api::DeploymentFailureView`. */
export interface DeploymentFailureView {
  code: string;
  phase: OperationPhase;
  message: string;
}

/** `ship_core::api::DeploymentView`. */
export interface DeploymentView {
  id: string;
  environment: string;
  action: DeploymentAction;
  revision_id: string;
  release_id: string | null;
  previous_release_id: string | null;
  operation_id: string;
  result: DeploymentResult | null;
  failure: DeploymentFailureView | null;
  started_at: string;
  ended_at: string | null;
}

/** `GET /api/v1/projects/{p}/deployments`, newest first. */
export interface DeploymentList {
  deployments: DeploymentView[];
}

/** One NDJSON line of `GET /api/v1/projects/{p}/logs`. */
export interface LogLineView {
  service: string;
  /** `stdout` or `stderr`. */
  stream: string;
  ts: string;
  line: string;
}

/** `ship_core::api::VarView`. */
export interface VarView {
  name: string;
  value: string;
  updated_at: string;
}

/** `ship_core::api::SecretView`: a name and when it was set, never a value. */
export interface SecretView {
  name: string;
  updated_at: string;
}

interface ScopeFields {
  scope: ConfigScope;
  project: string;
  environment: string | null;
  service: string | null;
  stale_services: string[];
}

/** `GET /api/v1/projects/{p}/vars`. */
export interface VarList extends ScopeFields {
  vars: VarView[];
}

/** `GET /api/v1/projects/{p}/secrets`. */
export interface SecretList extends ScopeFields {
  secrets: SecretView[];
}

/** `ship_core::api::OperationFailureView`. */
export interface OperationFailureView {
  code: string;
  phase: OperationPhase;
  message: string;
}

/** `GET /api/v1/operations/{id}`, and the `202` answer of a mutation. */
export interface OperationView {
  id: string;
  kind: string;
  environment_id: string | null;
  phase: OperationPhase;
  status: OperationStatus;
  failure: OperationFailureView | null;
  log_bytes: number;
  created_at: string;
  updated_at: string;
}

/** One SSE `data:` line of `GET /api/v1/operations/{id}/events`. */
export interface OperationEventView {
  operation_id: string;
  /** From 1, no gaps; also the SSE `id:`. */
  sequence: number;
  phase: OperationPhase;
  event: string;
  message: string;
  ts: string;
}

/** `ship_core::ApiError`. `code` is open: new codes may be added. */
export interface ApiError {
  code: string;
  message: string;
  details: Record<string, unknown>;
  operation_id: string | null;
}

type Check = (v: unknown) => boolean;

const isString: Check = (v) => typeof v === 'string';
const isNullableString: Check = (v) => v === null || typeof v === 'string';
const isNullableCount: Check = (v) =>
  v === null || (typeof v === 'number' && Number.isSafeInteger(v) && v >= 0);

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/** `v` is an object with exactly the keys of `shape`, each passing its check. */
function exact(v: unknown, shape: Record<string, Check>): v is Record<string, unknown> {
  if (!isObject(v)) return false;
  const keys = Object.keys(v);
  if (keys.length !== Object.keys(shape).length) return false;
  return keys.every((k) => Object.hasOwn(shape, k) && (shape[k] as Check)(v[k]));
}

export function isDockerStatus(v: unknown): v is DockerStatus {
  return typeof v === 'string' && (DOCKER_STATUSES as readonly string[]).includes(v);
}

/** A status the UI cannot name is rejected, never shown as another. */
export function isServiceStatus(v: unknown): v is ServiceStatus {
  return typeof v === 'string' && (SERVICE_STATUSES as readonly string[]).includes(v);
}

const isSupported: Check = (v) => exact(v, { engine: isString, compose: isString });

const isDockerInfo: Check = (v) =>
  exact(v, {
    status: isDockerStatus,
    engine_version: isNullableString,
    compose_version: isNullableString,
    supported: isSupported,
    message: isNullableString,
  });

const isCapacity: Check = (v) =>
  exact(v, {
    cpus: isNullableCount,
    memory_bytes: isNullableCount,
    disk_free_bytes: isNullableCount,
  });

export function isHostInfo(v: unknown): v is HostInfo {
  return exact(v, {
    api_version: isString,
    version: isString,
    os: isString,
    arch: isString,
    docker: isDockerInfo,
    capacity: isCapacity,
  });
}

const isEnvironmentSummary: Check = (v) =>
  exact(v, {
    id: isString,
    name: isString,
    desired_state: isString,
    status: (s) => s === null || isServiceStatus(s),
  });

const isProjectSummary: Check = (v) =>
  exact(v, {
    id: isString,
    slug: isString,
    created_at: isString,
    environments: (e) => Array.isArray(e) && e.every(isEnvironmentSummary),
  });

export function isProjectList(v: unknown): v is ProjectList {
  return exact(v, {
    projects: (p) => Array.isArray(p) && p.every(isProjectSummary),
  });
}

function oneOf(values: readonly string[]): Check {
  return (v) => typeof v === 'string' && values.includes(v);
}

function nullable(check: Check): Check {
  return (v) => v === null || check(v);
}

function arrayOf(check: Check): Check {
  return (v) => Array.isArray(v) && v.every(check);
}

const isBoolean: Check = (v) => typeof v === 'boolean';
const isCount: Check = (v) => typeof v === 'number' && Number.isSafeInteger(v) && v >= 0;
const isDesiredState = oneOf(DESIRED_STATES);
const isPhase = oneOf(OPERATION_PHASES);
const isResult = nullable(oneOf(DEPLOYMENT_RESULTS));
const isAction = oneOf(DEPLOYMENT_ACTIONS);

const isLastDeployment: Check = (v) =>
  exact(v, {
    id: isString,
    action: isAction,
    release_id: isNullableString,
    operation_id: isString,
    result: isResult,
    started_at: isString,
    ended_at: isNullableString,
  });

const isServiceStatusView: Check = (v) =>
  exact(v, {
    id: isString,
    name: isString,
    kind: isString,
    desired_state: isDesiredState,
    status: isServiceStatus,
    restarts: isCount,
    observed_at: isString,
    stale_config: isBoolean,
    endpoints: arrayOf((e) => exact(e, { provider: isString, address: isString })),
  });

export function isStatusView(v: unknown): v is StatusView {
  return exact(v, {
    project: (p) => exact(p, { id: isString, slug: isString }),
    environment: (e) => exact(e, { id: isString, name: isString, desired_state: isDesiredState }),
    last_deployment: nullable(isLastDeployment),
    services: arrayOf(isServiceStatusView),
  });
}

const isFailure: Check = (v) => exact(v, { code: isString, phase: isPhase, message: isString });

const isDeploymentView: Check = (v) =>
  exact(v, {
    id: isString,
    environment: isString,
    action: isAction,
    revision_id: isString,
    release_id: isNullableString,
    previous_release_id: isNullableString,
    operation_id: isString,
    result: isResult,
    failure: nullable(isFailure),
    started_at: isString,
    ended_at: isNullableString,
  });

export function isDeploymentList(v: unknown): v is DeploymentList {
  return exact(v, { deployments: arrayOf(isDeploymentView) });
}

export function isLogLine(v: unknown): v is LogLineView {
  return exact(v, { service: isString, stream: isString, ts: isString, line: isString });
}

function scoped(entries: string, entry: Check): Record<string, Check> {
  return {
    scope: oneOf(CONFIG_SCOPES),
    project: isString,
    environment: isNullableString,
    service: isNullableString,
    [entries]: arrayOf(entry),
    stale_services: arrayOf(isString),
  };
}

export function isVarList(v: unknown): v is VarList {
  return exact(
    v,
    scoped('vars', (e) => exact(e, { name: isString, value: isString, updated_at: isString })),
  );
}

/** Exactly a name and a time per secret: a list carrying a value is refused. */
export function isSecretList(v: unknown): v is SecretList {
  return exact(
    v,
    scoped('secrets', (e) => exact(e, { name: isString, updated_at: isString })),
  );
}

export function isOperationView(v: unknown): v is OperationView {
  return exact(v, {
    id: isString,
    kind: isString,
    environment_id: isNullableString,
    phase: isPhase,
    status: oneOf(OPERATION_STATUSES),
    failure: nullable(isFailure),
    log_bytes: isCount,
    created_at: isString,
    updated_at: isString,
  });
}

export function isOperationEvent(v: unknown): v is OperationEventView {
  return exact(v, {
    operation_id: isString,
    sequence: (s) => isCount(s) && (s as number) >= 1,
    phase: isPhase,
    event: isString,
    message: isString,
    ts: isString,
  });
}

/** Whether an operation phase is final. */
export function isTerminalPhase(phase: OperationPhase): boolean {
  return phase === 'succeeded' || phase === 'failed' || phase === 'interrupted';
}

// ---- paths ----

function seg(s: string): string {
  return encodeURIComponent(s);
}

/** `?k=v&…` of the pairs whose value is set; empty when none is. */
export function query(pairs: Record<string, string | null | undefined>): string {
  const parts = Object.entries(pairs)
    .filter((p): p is [string, string] => typeof p[1] === 'string')
    .map(([k, v]) => `${k}=${encodeURIComponent(v)}`);
  return parts.length === 0 ? '' : `?${parts.join('&')}`;
}

export function statusPath(project: string): string {
  return `${PROJECTS_PATH}/${seg(project)}/status`;
}

export function deploymentsPath(project: string): string {
  return `${PROJECTS_PATH}/${seg(project)}/deployments`;
}

/** The logs of one service, or of all with `service` null. */
export function logsPath(
  project: string,
  service: string | null,
  follow: boolean,
  tail: number,
): string {
  return `${PROJECTS_PATH}/${seg(project)}/logs${query({
    service,
    follow: follow ? 'true' : null,
    tail: String(tail),
  })}`;
}

/** A scope's config values: the project's, an environment's or a service's. */
export function varsPath(project: string, env: string | null, service: string | null): string {
  return `${PROJECTS_PATH}/${seg(project)}/vars${query({ env, service })}`;
}

/** A scope's secrets, names and times only. */
export function secretsPath(project: string, env: string | null, service: string | null): string {
  return `${PROJECTS_PATH}/${seg(project)}/secrets${query({ env, service })}`;
}

export function operationPath(id: string): string {
  return `/api/v1/operations/${seg(id)}`;
}

export function operationEventsPath(id: string): string {
  return `/api/v1/operations/${seg(id)}/events`;
}

export function isApiError(v: unknown): v is ApiError {
  return exact(v, {
    code: isString,
    message: isString,
    details: isObject,
    operation_id: isNullableString,
  });
}

/** Why a request failed, in words for the page. */
export class RequestError extends Error {
  constructor(
    message: string,
    readonly status: number | null,
  ) {
    super(message);
    this.name = 'RequestError';
  }
}

/**
 * `GET path` from the daemon that served this page, accepting the body only
 * if `guard` does. Throws a [`RequestError`] otherwise, carrying the
 * daemon's own message when it sent an `ApiError`.
 */
export async function getJson<T>(
  path: string,
  guard: (v: unknown) => v is T,
  fetchFn: typeof fetch = fetch,
): Promise<T> {
  let response: Response;
  try {
    response = await fetchFn(path, {
      method: 'GET',
      headers: { [API_VERSION_HEADER]: API_VERSION, Accept: 'application/json' },
      credentials: 'same-origin',
      cache: 'no-store',
    });
  } catch {
    throw new RequestError('The daemon did not answer.', null);
  }
  let body: unknown;
  try {
    body = await response.json();
  } catch {
    throw new RequestError(
      `The daemon answered HTTP ${response.status} without JSON.`,
      response.status,
    );
  }
  if (!response.ok) {
    if (isApiError(body)) {
      throw new RequestError(`${body.message} (${body.code})`, response.status);
    }
    throw new RequestError(`The daemon answered HTTP ${response.status}.`, response.status);
  }
  if (!guard(body)) {
    throw new RequestError(UNKNOWN_SHAPE, response.status);
  }
  return body;
}

const UNKNOWN_SHAPE =
  'The daemon answered with a shape this UI does not know; the UI and daemon may be from different builds.';

/** A fresh idempotency key: a UUID, 36 visible ASCII characters. */
export function idempotencyKey(): string {
  return crypto.randomUUID();
}

/**
 * `POST path` with no body and a fresh `Idempotency-Key`, as a lifecycle
 * action is sent (ADR-0011): answered `202` with the operation. Same-origin
 * only, so the browser sends `Origin` and the daemon's guard can check it.
 */
export async function postAction(
  path: string,
  fetchFn: typeof fetch = fetch,
  key: string = idempotencyKey(),
): Promise<OperationView> {
  let response: Response;
  try {
    response = await fetchFn(path, {
      method: 'POST',
      headers: {
        [API_VERSION_HEADER]: API_VERSION,
        [IDEMPOTENCY_KEY_HEADER]: key,
        Accept: 'application/json',
      },
      credentials: 'same-origin',
      cache: 'no-store',
    });
  } catch {
    throw new RequestError('The daemon did not answer.', null);
  }
  let body: unknown;
  try {
    body = await response.json();
  } catch {
    throw new RequestError(
      `The daemon answered HTTP ${response.status} without JSON.`,
      response.status,
    );
  }
  if (!response.ok) {
    if (isApiError(body)) {
      throw new RequestError(`${body.message} (${body.code})`, response.status);
    }
    throw new RequestError(`The daemon answered HTTP ${response.status}.`, response.status);
  }
  if (!isOperationView(body)) throw new RequestError(UNKNOWN_SHAPE, response.status);
  return body;
}
