<script lang="ts">
  import { onMount } from 'svelte';

  import { HOST_PATH, PROJECTS_PATH, isHostInfo, isProjectList } from './api';
  import type { HostInfo, ProjectList } from './api';
  import HostCard from './HostCard.svelte';
  import { load } from './load';
  import { Poller } from './poller.svelte';
  import Projects from './Projects.svelte';
  import RefreshButton from './RefreshButton.svelte';
  import { announcement, hostView, projectsView, type Load } from './view';

  const POLL_MS = 10_000;

  let host: Load<HostInfo> = $state({ kind: 'loading' });
  let projects: Load<ProjectList> = $state({ kind: 'loading' });

  const hv = $derived(hostView(host));
  const pv = $derived(projectsView(projects));
  const said = $derived(announcement(hv, pv));

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

  onMount(() => poller.start(POLL_MS));
</script>

<div class="flex items-center justify-between gap-4">
  <h1 class="text-xl font-semibold" tabindex="-1" data-page-heading>Overview</h1>
  <RefreshButton refreshing={poller.refreshing} onrefresh={() => void poller.refresh()} />
</div>
<HostCard view={hv} />
<Projects view={pv} />
<p role="status" aria-live="polite" aria-atomic="true" class="sr-only">{said}</p>
