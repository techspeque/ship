<script lang="ts">
  import { onMount } from 'svelte';

  import {
    LABELS,
    actionsFor,
    confirmation,
    needsConfirmation,
    submitAction,
    type Action,
    type Target,
  } from './actions';
  import {
    deploymentsPath,
    isDeploymentList,
    isStatusView,
    statusPath,
    type DeploymentList,
    type StatusView,
  } from './api';
  import Badge from './Badge.svelte';
  import Config from './Config.svelte';
  import ConfirmDialog from './ConfirmDialog.svelte';
  import { load, loadConfig } from './load';
  import { Poller } from './poller.svelte';
  import { deploymentRows, statusPage, type ConfigSection } from './project';
  import RefreshButton from './RefreshButton.svelte';
  import { href } from './route';
  import { FOCUS, TONE_CLASSES } from './tone';
  import type { Load } from './view';

  let { project }: { project: string } = $props();

  const POLL_MS = 5_000;

  let status = $state<Load<StatusView>>({ kind: 'loading' });
  let deployments = $state<Load<DeploymentList>>({ kind: 'loading' });
  let config = $state<Load<ConfigSection[]>>({ kind: 'loading' });
  let pending = $state<{ action: Action; target: Target } | null>(null);
  let sending = $state(false);
  let actionError = $state<string | null>(null);

  const page = $derived(status.kind === 'ok' ? statusPage(status.value) : null);
  const history = $derived(deployments.kind === 'ok' ? deploymentRows(deployments.value) : null);
  const asked = $derived(pending === null ? null : confirmation(pending.action, pending.target));

  let configLoaded = false;

  const poller = new Poller(async () => {
    const [s, d] = await Promise.all([
      load(statusPath(project), isStatusView),
      load(deploymentsPath(project), isDeploymentList),
    ]);
    status = s;
    deployments = d;
    if (s.kind === 'ok' && !configLoaded) {
      configLoaded = true;
      void refreshConfig(s.value);
    }
  });

  async function refreshConfig(s: StatusView): Promise<void> {
    config = await loadConfig(
      project,
      s.environment.name,
      s.services.map((v) => v.name),
    );
  }

  async function refreshAll(): Promise<void> {
    await poller.refresh();
    if (status.kind === 'ok') await refreshConfig(status.value);
  }

  function target(service: string | null): Target | null {
    if (page === null) return null;
    return {
      project,
      environment: page.environment,
      service,
      services: page.services.map((s) => s.name),
    };
  }

  function ask(action: Action, service: string | null): void {
    const t = target(service);
    if (t === null) return;
    actionError = null;
    if (needsConfirmation(action)) pending = { action, target: t };
    else void send(action, t, false);
  }

  async function send(action: Action, t: Target, confirmed: boolean): Promise<void> {
    sending = true;
    try {
      const op = await submitAction(action, t, confirmed);
      location.hash = href({ page: 'operation', project, id: op.id });
    } catch (e) {
      actionError = e instanceof Error ? e.message : String(e);
    } finally {
      sending = false;
    }
  }

  function confirmPending(): void {
    const p = pending;
    pending = null;
    if (p !== null) void send(p.action, p.target, true);
  }

  onMount(() => poller.start(POLL_MS));

  const CARD =
    'rounded-lg border border-slate-200 bg-white p-5 shadow-sm dark:border-slate-700 dark:bg-slate-900';
  const BUTTON = `rounded-md border border-slate-300 bg-white px-2.5 py-1 text-sm font-medium hover:bg-slate-100 disabled:opacity-60 dark:border-slate-600 dark:bg-slate-800 dark:hover:bg-slate-700 ${FOCUS}`;
  const LINK = `rounded-sm text-sky-700 underline hover:text-sky-900 dark:text-sky-300 ${FOCUS}`;
</script>

<div class="flex flex-wrap items-center justify-between gap-4">
  <div>
    <a href={href({ page: 'home' })} class="text-sm {LINK}">All projects</a>
    <h1 class="text-xl font-semibold" tabindex="-1" data-page-heading>{project}</h1>
  </div>
  <RefreshButton refreshing={poller.refreshing} onrefresh={() => void refreshAll()} />
</div>

{#if actionError !== null}
  <p role="alert" class="rounded-md px-3 py-2 text-sm ring-1 {TONE_CLASSES.bad}">
    <span aria-hidden="true" class="font-mono">✕</span>
    {actionError}
  </p>
{/if}

<section aria-labelledby="status-heading" class={CARD}>
  <h2 id="status-heading" class="text-lg font-semibold">Status</h2>
  {#if status.kind === 'loading'}
    <p class="mt-3 text-sm">Loading status…</p>
  {:else if status.kind === 'error'}
    <p class="mt-3 rounded-md px-3 py-2 text-sm ring-1 {TONE_CLASSES.bad}">
      <span aria-hidden="true" class="font-mono">✕</span>
      {status.message}
    </p>
  {:else if page !== null}
    <dl class="mt-3 grid grid-cols-1 gap-x-6 gap-y-2 text-sm sm:grid-cols-[max-content_1fr]">
      <dt class="text-slate-500 dark:text-slate-400">Environment</dt>
      <dd class="font-mono">{page.environment}</dd>
      <dt class="text-slate-500 dark:text-slate-400">Desired state</dt>
      <dd class="font-mono" data-field="environment-desired">{page.desired}</dd>
      <dt class="text-slate-500 dark:text-slate-400">Last deployment</dt>
      <dd>
        {#if page.lastDeployment === null}
          none yet
        {:else}
          <span class="font-mono">{page.lastDeployment.action}</span>
          <Badge badge={page.lastDeployment.result} />
          started <time datetime={page.lastDeployment.startedAt}>{page.lastDeployment.startedAt}</time
          >{#if page.lastDeployment.endedAt !== null}, ended <time
              datetime={page.lastDeployment.endedAt}>{page.lastDeployment.endedAt}</time
            >{/if}
          ·
          <a
            class={LINK}
            href={href({ page: 'operation', project, id: page.lastDeployment.operationId })}
            >its operation</a
          >
        {/if}
      </dd>
    </dl>

    {#if page.interruption !== null}
      <p
        class="mt-3 rounded-md px-3 py-2 text-sm ring-1 {TONE_CLASSES.warn}"
        data-field="interruption"
      >
        <span aria-hidden="true" class="font-mono">!</span>
        A deployment was interrupted while
        <span class="font-mono">{page.interruption.phase}</span>; the services below show what
        Docker runs.
        <a
          class={LINK}
          href={href({ page: 'operation', project, id: page.interruption.operationId })}
          >Its operation</a
        >. Put it right with <code class="font-mono">{page.interruption.command}</code>.
      </p>
    {/if}

    <div class="mt-4 flex flex-wrap gap-2" role="group" aria-label="Actions on {project}">
      {#each actionsFor({ project, environment: page.environment, service: null, services: [] }) as a (a)}
        <button type="button" class={BUTTON} disabled={sending} onclick={() => ask(a, null)}>
          {LABELS[a]}
        </button>
      {/each}
      <a class="self-center text-sm {LINK}" href={href({ page: 'logs', project, service: null })}
        >All logs</a
      >
    </div>

    <h3 class="mt-5 font-medium">Services</h3>
    {#if page.services.length === 0}
      <p class="text-sm text-slate-500 dark:text-slate-400">No services yet.</p>
    {:else}
      <div class="overflow-x-auto">
        <table class="mt-1 w-full text-left text-sm">
          <thead class="text-slate-500 dark:text-slate-400">
            <tr>
              <th scope="col" class="py-1 pr-3 font-normal">Service</th>
              <th scope="col" class="py-1 pr-3 font-normal">Desired</th>
              <th scope="col" class="py-1 pr-3 font-normal">Observed</th>
              <th scope="col" class="py-1 pr-3 font-normal">Restarts</th>
              <th scope="col" class="py-1 pr-3 font-normal">Endpoints</th>
              <th scope="col" class="py-1 font-normal">Actions</th>
            </tr>
          </thead>
          <tbody>
            {#each page.services as s (s.id)}
              <tr class="border-t border-slate-200 align-top dark:border-slate-700">
                <th scope="row" class="py-2 pr-3 font-normal">
                  <span class="font-medium">{s.name}</span>
                  <span class="text-slate-500 dark:text-slate-400">({s.kind})</span>
                  {#if s.staleConfig}
                    <span class="block text-xs {TONE_CLASSES.warn} rounded px-1 ring-1"
                      >config changed since deploy; redeploy to apply</span
                    >
                  {/if}
                </th>
                <td class="py-2 pr-3 font-mono" data-field="service-desired">{s.desired}</td>
                <td class="py-2 pr-3" data-field="service-observed">
                  <Badge badge={s.observed} />
                  <span class="block text-xs text-slate-500 dark:text-slate-400">
                    at <time datetime={s.observedAt}>{s.observedAt}</time>
                  </span>
                </td>
                <td class="py-2 pr-3 font-mono">{s.restarts}</td>
                <td class="py-2 pr-3 font-mono">
                  {s.endpoints.length === 0 ? 'not configured' : s.endpoints.join(', ')}
                </td>
                <td class="py-2">
                  <div class="flex flex-wrap gap-1" role="group" aria-label="Actions on {s.name}">
                    {#each actionsFor( { project, environment: page.environment, service: s.name, services: [] }, ) as a (a)}
                      <button
                        type="button"
                        class={BUTTON}
                        disabled={sending}
                        onclick={() => ask(a, s.name)}
                        aria-label="{LABELS[a]} {s.name}"
                      >
                        {LABELS[a]}
                      </button>
                    {/each}
                    <a
                      class="self-center {LINK}"
                      href={href({ page: 'logs', project, service: s.name })}
                      aria-label="Logs of {s.name}">Logs</a
                    >
                  </div>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  {/if}
</section>

<section aria-labelledby="history-heading" class={CARD}>
  <h2 id="history-heading" class="text-lg font-semibold">Deployment history</h2>
  {#if deployments.kind === 'loading'}
    <p class="mt-3 text-sm">Loading deployments…</p>
  {:else if deployments.kind === 'error'}
    <p class="mt-3 rounded-md px-3 py-2 text-sm ring-1 {TONE_CLASSES.bad}">
      <span aria-hidden="true" class="font-mono">✕</span>
      {deployments.message}
    </p>
  {:else if history !== null && history.length === 0}
    <p class="mt-3 text-sm text-slate-500 dark:text-slate-400">No deployments yet.</p>
  {:else if history !== null}
    <ol class="mt-3 divide-y divide-slate-200 text-sm dark:divide-slate-700">
      {#each history as d (d.id)}
        <li class="py-2">
          <div class="flex flex-wrap items-center gap-2">
            <span class="font-mono">{d.action}</span>
            <Badge badge={d.result} />
            <time datetime={d.startedAt}>{d.startedAt}</time>
            <a class={LINK} href={href({ page: 'operation', project, id: d.operationId })}
              >operation and log</a
            >
          </div>
          <p class="font-mono text-xs text-slate-500 dark:text-slate-400">
            revision {d.revision}{#if d.release !== null}, release {d.release}{/if}
          </p>
          {#if d.failure !== null}
            <p class="mt-1 rounded-md px-2 py-1 ring-1 {TONE_CLASSES.bad}">
              <span class="font-mono">{d.failure.code}</span> in
              <span class="font-mono">{d.failure.phase}</span>: {d.failure.message}
            </p>
          {/if}
        </li>
      {/each}
    </ol>
  {/if}
</section>

<Config {config} />

<ConfirmDialog
  confirmation={asked}
  onconfirm={confirmPending}
  oncancel={() => (pending = null)}
/>
