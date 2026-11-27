<script lang="ts">
  import { onMount } from 'svelte';

  import { HOST_PATH, PROJECTS_PATH, getJson, isHostInfo, isProjectList } from './lib/api';
  import type { HostInfo, ProjectList } from './lib/api';
  import HostCard from './lib/HostCard.svelte';
  import Projects from './lib/Projects.svelte';
  import { Poller } from './lib/poller.svelte';
  import { FOCUS } from './lib/tone';
  import { announcement, hostView, projectsView, type Load } from './lib/view';

  const POLL_MS = 10_000;

  let host: Load<HostInfo> = $state({ kind: 'loading' });
  let projects: Load<ProjectList> = $state({ kind: 'loading' });

  const hv = $derived(hostView(host));
  const pv = $derived(projectsView(projects));
  const said = $derived(announcement(hv, pv));

  async function load<T>(path: string, guard: (v: unknown) => v is T): Promise<Load<T>> {
    try {
      return { kind: 'ok', value: await getJson(path, guard) };
    } catch (e) {
      return { kind: 'error', message: e instanceof Error ? e.message : String(e) };
    }
  }

  // The last answer stays on screen until the next one arrives, so a
  // refresh never flashes back to "loading".
  const poller = new Poller(async () => {
    const [h, p] = await Promise.all([
      load(HOST_PATH, isHostInfo),
      load(PROJECTS_PATH, isProjectList),
    ]);
    host = h;
    projects = p;
  });
  const refreshing = $derived(poller.refreshing);

  function refresh(): Promise<void> {
    return poller.refresh();
  }

  onMount(() => poller.start(POLL_MS));
</script>

<a
  href="#main"
  class="sr-only rounded-md bg-white px-3 py-2 text-sm font-medium text-slate-900 focus:not-sr-only focus:absolute focus:top-2 focus:left-2 dark:bg-slate-900 dark:text-slate-100 {FOCUS}"
>
  Skip to main content
</a>

<div
  class="min-h-screen bg-slate-50 font-sans text-slate-900 dark:bg-slate-950 dark:text-slate-100"
>
  <header class="border-b border-slate-200 bg-white dark:border-slate-800 dark:bg-slate-900">
    <div class="mx-auto flex max-w-4xl items-center justify-between gap-4 px-4 py-3">
      <a href="/" class="rounded-sm text-xl font-bold tracking-tight {FOCUS}">Ship</a>
      <button
        type="button"
        onclick={() => void refresh()}
        disabled={refreshing}
        aria-busy={refreshing}
        class="rounded-md border border-slate-300 bg-white px-3 py-1.5 text-sm font-medium hover:bg-slate-100 disabled:opacity-60 dark:border-slate-600 dark:bg-slate-800 dark:hover:bg-slate-700 {FOCUS}"
      >
        {refreshing ? 'Refreshing…' : 'Refresh'}
      </button>
    </div>
  </header>

  <main id="main" tabindex="-1" class="mx-auto grid max-w-4xl gap-6 px-4 py-6 outline-none">
    <HostCard view={hv} />
    <Projects view={pv} />
  </main>

  <p role="status" aria-live="polite" aria-atomic="true" class="sr-only">{said}</p>
</div>
