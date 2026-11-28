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
    throw new RequestError(
      'The daemon answered with a shape this UI does not know; the UI and daemon may be from different builds.',
      response.status,
    );
  }
  return body;
}
