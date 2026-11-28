<script lang="ts">
  import { onMount, tick } from 'svelte';

  import Home from './lib/Home.svelte';
  import LogsPage from './lib/LogsPage.svelte';
  import OperationPage from './lib/OperationPage.svelte';
  import ProjectPage from './lib/ProjectPage.svelte';
  import { href, parseRoute, type Route } from './lib/route';
  import { FOCUS } from './lib/tone';

  let route: Route = $state(parseRoute(location.hash));

  // On a route change, focus moves to the new page's heading, so keyboard
  // and screen-reader users start at the top of what changed.
  onMount(() => {
    const onHash = async (): Promise<void> => {
      route = parseRoute(location.hash);
      await tick();
      document.querySelector<HTMLElement>('[data-page-heading]')?.focus();
    };
    const listener = (): void => void onHash();
    addEventListener('hashchange', listener);
    return () => removeEventListener('hashchange', listener);
  });

  // Each page is keyed by its route, so moving between two projects starts
  // the second page fresh instead of reusing the first one's state.
  const key = $derived(href(route));
</script>

<a
  href="#main"
  onclick={(e) => {
    e.preventDefault();
    document.getElementById('main')?.focus();
  }}
  class="sr-only rounded-md bg-white px-3 py-2 text-sm font-medium text-slate-900 focus:not-sr-only focus:absolute focus:top-2 focus:left-2 dark:bg-slate-900 dark:text-slate-100 {FOCUS}"
>
  Skip to main content
</a>

<div
  class="min-h-screen bg-slate-50 font-sans text-slate-900 dark:bg-slate-950 dark:text-slate-100"
>
  <header class="border-b border-slate-200 bg-white dark:border-slate-800 dark:bg-slate-900">
    <div class="mx-auto flex max-w-5xl items-center justify-between gap-4 px-4 py-3">
      <a href={href({ page: 'home' })} class="rounded-sm text-xl font-bold tracking-tight {FOCUS}"
        >Ship</a
      >
    </div>
  </header>

  <main id="main" tabindex="-1" class="mx-auto grid max-w-5xl gap-6 px-4 py-6 outline-none">
    {#key key}
      {#if route.page === 'home'}
        <Home />
      {:else if route.page === 'project'}
        <ProjectPage project={route.project} />
      {:else if route.page === 'logs'}
        <LogsPage project={route.project} service={route.service} />
      {:else if route.page === 'operation'}
        <OperationPage project={route.project} id={route.id} />
      {:else}
        <h1 class="text-xl font-semibold" tabindex="-1" data-page-heading>No such page</h1>
        <p><a class="underline {FOCUS}" href={href({ page: 'home' })}>Back to the overview</a></p>
      {/if}
    {/key}
  </main>
</div>
