import { getJson, isSecretList, isVarList, secretsPath, varsPath } from './api';
import { configSection, type ConfigSection } from './project';
import type { Load } from './view';

/**
 * Every config scope of a project's environment: the project's, the
 * environment's and each service's, each with its values and its secret
 * names. Reads only; the UI never sends a secret.
 */
export async function loadConfig(
  project: string,
  environment: string,
  services: readonly string[],
  fetchFn: typeof fetch = fetch,
): Promise<Load<ConfigSection[]>> {
  const scopes: [string | null, string | null][] = [
    [null, null],
    [environment, null],
    ...services.map((s): [string, string] => [environment, s]),
  ];
  try {
    const sections = await Promise.all(
      scopes.map(async ([env, service]) => {
        const [vars, secrets] = await Promise.all([
          getJson(varsPath(project, env, service), isVarList, fetchFn),
          getJson(secretsPath(project, env, service), isSecretList, fetchFn),
        ]);
        return configSection(vars, secrets);
      }),
    );
    return { kind: 'ok', value: sections };
  } catch (e) {
    return { kind: 'error', message: e instanceof Error ? e.message : String(e) };
  }
}

/** `GET path` as a [`Load`]: the value, or the error's message. */
export async function load<T>(
  path: string,
  guard: (v: unknown) => v is T,
  fetchFn: typeof fetch = fetch,
): Promise<Load<T>> {
  try {
    return { kind: 'ok', value: await getJson(path, guard, fetchFn) };
  } catch (e) {
    return { kind: 'error', message: e instanceof Error ? e.message : String(e) };
  }
}
