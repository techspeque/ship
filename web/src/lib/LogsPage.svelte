<script lang="ts">
  import { onMount } from 'svelte';

  import { isStatusView, logsPath, statusPath, type LogLineView } from './api';
  import { load } from './load';
  import { streamLogs, type StreamEnd } from './ndjson';
  import { href } from './route';
  import { FOCUS, TONE_CLASSES } from './tone';

  let { project, service }: { project: string; service: string | null } = $props();

  /** Lines requested from the end of each service's log. */
  const TAIL = 200;
  /** Lines kept on the page; older ones are dropped while following. */
  const KEEP = 5_000;

  let services = $state<string[]>([]);
  let follow = $state(true);
  let lines = $state<(LogLineView & { n: number })[]>([]);
  let end = $state<StreamEnd | null>(null);
  let error = $state<string | null>(null);
  let n = 0;

  onMount(() => {
    void load(statusPath(project), isStatusView).then((s) => {
      if (s.kind === 'ok') services = s.value.services.map((v) => v.name);
    });
  });

  // One stream per (service, follow); a change aborts the last one.
  $effect(() => {
    const path = logsPath(project, service, follow, TAIL);
    const stop = new AbortController();
    lines = [];
    end = null;
    error = null;
    void streamLogs(
      path,
      (l) => {
        lines.push({ ...l, n: n++ });
        if (lines.length > KEEP) lines.splice(0, lines.length - KEEP);
      },
      stop.signal,
    )
      .then((e) => {
        if (!stop.signal.aborted) end = e;
      })
      .catch((e: unknown) => {
        if (!stop.signal.aborted) error = e instanceof Error ? e.message : String(e);
      });
    return () => stop.abort();
  });

  const END: Record<StreamEnd, string> = {
    complete: 'End of the logs.',
    cut_short:
      'The logs were cut short: Ship could not read all of them, so this is not everything.',
    aborted: 'Stopped.',
  };
  const LINK = `rounded-sm text-sky-700 underline hover:text-sky-900 dark:text-sky-300 ${FOCUS}`;
  const CURRENT = 'font-semibold no-underline';
</script>

<div>
  <a href={href({ page: 'project', project })} class="text-sm {LINK}">Back to {project}</a>
  <h1 class="text-xl font-semibold" tabindex="-1" data-page-heading>
    Logs of {service ?? `every service of ${project}`}
  </h1>
</div>

<section
  aria-labelledby="logs-heading"
  class="rounded-lg border border-slate-200 bg-white p-5 shadow-sm dark:border-slate-700 dark:bg-slate-900"
>
  <h2 id="logs-heading" class="sr-only">Log lines</h2>
  <div class="flex flex-wrap items-center gap-3 text-sm">
    <nav aria-label="Service">
      <ul class="flex flex-wrap gap-3">
        <li>
          <a
            href={href({ page: 'logs', project, service: null })}
            class="{LINK} {service === null ? CURRENT : ''}"
            aria-current={service === null ? 'page' : undefined}>All</a
          >
        </li>
        {#each services as s (s)}
          <li>
            <a
              href={href({ page: 'logs', project, service: s })}
              class="{LINK} {service === s ? CURRENT : ''}"
              aria-current={service === s ? 'page' : undefined}>{s}</a
            >
          </li>
        {/each}
      </ul>
    </nav>
    <label class="ml-auto inline-flex items-center gap-2">
      <input type="checkbox" bind:checked={follow} class={FOCUS} />
      Follow
    </label>
  </div>

  {#if error !== null}
    <p role="alert" class="mt-3 rounded-md px-3 py-2 text-sm ring-1 {TONE_CLASSES.bad}">
      <span aria-hidden="true" class="font-mono">✕</span>
      {error}
    </p>
  {/if}

  <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
  <ol
    role="log"
    aria-labelledby="logs-heading"
    tabindex="0"
    class="mt-3 max-h-[40rem] overflow-auto rounded-md bg-slate-950 p-3 font-mono text-xs text-slate-100 {FOCUS}"
  >
    {#each lines as l (l.n)}
      <li class="whitespace-pre-wrap break-words" data-stream={l.stream}>
        <span class="text-slate-400"><time datetime={l.ts}>{l.ts}</time></span>
        {#if service === null}<span class="text-sky-300">{l.service}</span>{/if}
        <span class={l.stream === 'stderr' ? 'text-amber-300' : 'text-slate-400'}>{l.stream}</span>
        <span class={l.stream === 'stderr' ? 'text-amber-100' : ''}>{l.line}</span>
      </li>
    {:else}
      <li class="text-slate-400">{end === null && error === null ? 'Waiting for lines…' : 'No lines.'}</li>
    {/each}
  </ol>
  {#if end !== null}
    <p
      class="mt-2 text-sm {end === 'cut_short'
        ? `rounded-md px-3 py-2 ring-1 ${TONE_CLASSES.warn}`
        : 'text-slate-500 dark:text-slate-400'}"
    >
      {END[end]}
    </p>
  {/if}
</section>
