import { describe, expect, it } from 'vitest';

import { href, parseRoute, type Route } from './route';

describe('routes', () => {
  it('parses every page and round-trips through href', () => {
    const routes: Route[] = [
      { page: 'home' },
      { page: 'project', project: 'my-app' },
      { page: 'logs', project: 'my-app', service: null },
      { page: 'logs', project: 'my-app', service: 'web worker' },
      { page: 'operation', project: 'my-app', id: 'op_01aryz6s41tsv4rrffq69g5fav' },
    ];
    for (const r of routes) expect(parseRoute(href(r))).toEqual(r);
  });

  it('reads an empty hash as home', () => {
    expect(parseRoute('')).toEqual({ page: 'home' });
    expect(parseRoute('#')).toEqual({ page: 'home' });
    expect(parseRoute('#/')).toEqual({ page: 'home' });
  });

  it('anything else is missing, never another page', () => {
    for (const h of [
      '#/nope',
      '#/projects',
      '#/projects/',
      '#/projects/a/b',
      '#/projects/a/operations',
      '#/projects/a/logs/w/x',
      '#/projects/%E0%A4%A',
      '#main',
    ]) {
      expect(parseRoute(h), h).toEqual({ page: 'missing', hash: h });
    }
  });
});
