<script lang="ts">
  import { onMount } from 'svelte';

  import { getJson, isOperationView, operationPath, type OperationView } from './api';
  import Badge from './Badge.svelte';
  import { watchOperation } from './operation';
  import { operationLine, operationPage, type OperationLine } from './project';
  import { href } from './route';
  import { followOperation, type Connection } from './sse';
  import { FOCUS, TONE_CLASSES } from './tone';

  let { project, id }: { project: string; id: string } = $props();

  let op = $state<OperationView | null>(null);
  let error = $state<string | null>(null);
  let lines = $state<OperationLine[]>([]);
  let phase = $state<string | null>(null);
  let connection = $state<Connection>('connecting');

  const view = $derived(op === null ? null : operationPage(op));

  onMount(() => {
    const stop = new AbortController();
    watchOperation({
      read: () => getJson(operationPath(id), isOperationView),
      follow: (onEvent, signal) =>
        followOperation(id, onEvent, { signal, onConnection: (c) => (connection = c) }),
      onOperation: (o) => (op = o),
      onEvent: (e) => {
        lines.push(operationLine(e));
        phase = e.phase;
      },
      signal: stop.signal,
    }).catch((e: unknown) => {
      if (!stop.signal.aborted) error = e instanceof Error ? e.message : String(e);
    });
    return () => stop.abort();
  });

  const CONNECTION: Record<Connection, string> = {
    connecting: 'connecting…',
    live: 'live',
    reconnecting: 'connection lost, reconnecting…',
    ended: 'ended',
  };
  const LINK = `rounded-sm text-sky-700 underline hover:text-sky-900 dark:text-sky-300 ${FOCUS}`;
</script>

<div>
  <a href={href({ page: 'project', project })} class="text-sm {LINK}">Back to {project}</a>
  <h1 class="text-xl font-semibold" tabindex="-1" data-page-heading>
    Operation <span class="font-mono text-base">{id}</span>
  </h1>
</div>

{#if error !== null}
  <p role="alert" class="rounded-md px-3 py-2 text-sm ring-1 {TONE_CLASSES.bad}">
    <span aria-hidden="true" class="font-mono">✕</span>
    {error}
  </p>
{/if}

<section
  aria-labelledby="op-heading"
  class="rounded-lg border border-slate-200 bg-white p-5 shadow-sm dark:border-slate-700 dark:bg-slate-900"
>
  <h2 id="op-heading" class="text-lg font-semibold">Progress</h2>
  {#if view === null}
    <p class="mt-3 text-sm">Loading the operation…</p>
  {:else}
    <dl class="mt-3 grid grid-cols-1 gap-x-6 gap-y-2 text-sm sm:grid-cols-[max-content_1fr]">
      <dt class="text-slate-500 dark:text-slate-400">Kind</dt>
      <dd class="font-mono">{view.kind}</dd>
      <dt class="text-slate-500 dark:text-slate-400">Phase</dt>
      <dd class="font-mono" aria-live="polite">{phase ?? view.phase}</dd>
      <dt class="text-slate-500 dark:text-slate-400">Status</dt>
      <dd><Badge badge={view.status} /></dd>
      <dt class="text-slate-500 dark:text-slate-400">Created</dt>
      <dd><time datetime={view.createdAt}>{view.createdAt}</time></dd>
      <dt class="text-slate-500 dark:text-slate-400">Events</dt>
      <dd>{CONNECTION[connection]}</dd>
    </dl>
    {#if view.failure !== null}
      <p class="mt-3 rounded-md px-3 py-2 text-sm ring-1 {TONE_CLASSES.bad}">
        <span class="font-mono">{view.failure.code}</span> in
        <span class="font-mono">{view.failure.phase}</span>: {view.failure.message}
      </p>
    {/if}
  {/if}
</section>

<section
  aria-labelledby="oplog-heading"
  class="rounded-lg border border-slate-200 bg-white p-5 shadow-sm dark:border-slate-700 dark:bg-slate-900"
>
  <h2 id="oplog-heading" class="text-lg font-semibold">Log</h2>
  <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
  <ol
    role="log"
    aria-labelledby="oplog-heading"
    tabindex="0"
    class="mt-3 max-h-[32rem] overflow-auto rounded-md bg-slate-950 p-3 font-mono text-xs text-slate-100 {FOCUS}"
  >
    {#each lines as l (l.sequence)}
      <li class="whitespace-pre-wrap break-words">
        <span class="text-slate-400">{l.phase}</span>
        {#if l.event !== 'log'}<span class="text-sky-300">[{l.event}]</span>{/if}
        {l.message}
      </li>
    {:else}
      <li class="text-slate-400">No events yet.</li>
    {/each}
  </ol>
</section>
