// The lifecycle actions the UI offers (phase-1-ws-1.12), which need a
// confirmation, what it says, and the request that carries them.

import { PROJECTS_PATH, postAction, query, type OperationView } from './api';

/** In the order the page shows them. */
export const ACTIONS = ['start', 'stop', 'restart', 'redeploy', 'remove'] as const;
export type Action = (typeof ACTIONS)[number];

/** What an action is aimed at. */
export interface Target {
  project: string;
  environment: string;
  /** One service, or `null` for the whole environment. */
  service: string | null;
  /** Every service of the environment, by name. */
  services: readonly string[];
}

export const LABELS: Record<Action, string> = {
  start: 'Start',
  stop: 'Stop',
  restart: 'Restart',
  redeploy: 'Redeploy',
  remove: 'Remove',
};

/**
 * `redeploy` and `remove` cover the whole environment (a release and a
 * Compose project cover every service), so a service offers only start,
 * stop and restart.
 */
export function actionsFor(target: Target): Action[] {
  return target.service === null ? [...ACTIONS] : ['start', 'stop', 'restart'];
}

/**
 * Every action but `start` interrupts what runs: `stop` and `remove` take
 * it down, `restart` and `redeploy` replace it. Those ask first.
 */
export function needsConfirmation(action: Action): boolean {
  return action !== 'start';
}

/** The confirmation dialog's words. */
export interface Confirmation {
  title: string;
  /** Sentences naming what is affected. */
  lines: string[];
  confirm: string;
}

function affected(target: Target): string {
  if (target.service !== null) return `service ${target.service}`;
  if (target.services.length === 0) return 'no service yet';
  const s = target.services.length === 1 ? 'service' : 'services';
  return `${s} ${target.services.join(', ')}`;
}

export function confirmation(action: Action, target: Target): Confirmation {
  const where = `${target.project} (${target.environment})`;
  const what = affected(target);
  switch (action) {
    case 'start':
      return { title: `Start ${where}?`, lines: [`Starts ${what}.`], confirm: 'Start' };
    case 'stop':
      return {
        title: `Stop ${where}?`,
        lines: [
          `Stops ${what}.`,
          'Ship will not start it again until you start it.',
        ],
        confirm: 'Stop',
      };
    case 'restart':
      return {
        title: `Restart ${where}?`,
        lines: [`Restarts the containers of ${what} on the current release.`],
        confirm: 'Restart',
      };
    case 'redeploy':
      return {
        title: `Redeploy ${where}?`,
        lines: [
          `Deploys the current revision again with the current config, replacing the containers of ${what}.`,
        ],
        confirm: 'Redeploy',
      };
    case 'remove':
      return {
        title: `Remove ${where}?`,
        lines: [
          `Removes the containers and networks of ${what}.`,
          'Named volumes, their data and the deployment history are kept.',
          'Only a push with --recreate brings the environment back.',
        ],
        confirm: 'Remove',
      };
  }
}

/** The request path of `action` on `target`. */
export function actionPath(action: Action, target: Target): string {
  return `${PROJECTS_PATH}/${encodeURIComponent(target.project)}/${action}${query({
    env: target.environment,
    service: target.service,
  })}`;
}

/** Why an action was not sent. */
export class ActionRefused extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'ActionRefused';
  }
}

/**
 * Sends `action`. One that needs a confirmation is refused, before any
 * request, unless `confirmed`: the dialog is the only caller that passes it.
 */
export function submitAction(
  action: Action,
  target: Target,
  confirmed: boolean,
  fetchFn: typeof fetch = fetch,
): Promise<OperationView> {
  if (!actionsFor(target).includes(action)) {
    return Promise.reject(new ActionRefused(`${action} applies to the whole environment only`));
  }
  if (needsConfirmation(action) && !confirmed) {
    return Promise.reject(new ActionRefused(`${action} needs a confirmation`));
  }
  return postAction(actionPath(action, target), fetchFn);
}
