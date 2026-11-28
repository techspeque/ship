// Client routes, in the URL's hash so the daemon serves one page for all
// of them and no route reaches the server.

export type Route =
  | { page: 'home' }
  | { page: 'project'; project: string }
  | { page: 'logs'; project: string; service: string | null }
  | { page: 'operation'; project: string; id: string }
  | { page: 'missing'; hash: string };

function decode(s: string): string | null {
  try {
    const v = decodeURIComponent(s);
    return v === '' ? null : v;
  } catch {
    return null;
  }
}

/** The route a `location.hash` names. */
export function parseRoute(hash: string): Route {
  const path = hash.replace(/^#/, '');
  if (path === '' || path === '/') return { page: 'home' };
  const parts = path.replace(/^\//, '').split('/');
  const [root, rawProject, sub, rawArg, ...rest] = parts;
  const project = rawProject === undefined ? null : decode(rawProject);
  if (root !== 'projects' || project === null || rest.length > 0) {
    return { page: 'missing', hash };
  }
  if (sub === undefined) return { page: 'project', project };
  if (sub === 'logs') {
    if (rawArg === undefined) return { page: 'logs', project, service: null };
    const service = decode(rawArg);
    if (service !== null) return { page: 'logs', project, service };
  }
  if (sub === 'operations' && rawArg !== undefined) {
    const id = decode(rawArg);
    if (id !== null) return { page: 'operation', project, id };
  }
  return { page: 'missing', hash };
}

/** The `href` of a route. */
export function href(route: Route): string {
  const e = encodeURIComponent;
  switch (route.page) {
    case 'home':
      return '#/';
    case 'project':
      return `#/projects/${e(route.project)}`;
    case 'logs':
      return route.service === null
        ? `#/projects/${e(route.project)}/logs`
        : `#/projects/${e(route.project)}/logs/${e(route.service)}`;
    case 'operation':
      return `#/projects/${e(route.project)}/operations/${e(route.id)}`;
    case 'missing':
      return route.hash;
  }
}
