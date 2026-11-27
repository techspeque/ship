<script lang="ts">
  import { TONE_CLASSES } from './tone';
  import type { HostView } from './view';

  let { view }: { view: HostView } = $props();
</script>

<section
  aria-labelledby="host-heading"
  class="rounded-lg border border-slate-200 bg-white p-5 shadow-sm dark:border-slate-700 dark:bg-slate-900"
>
  <h2 id="host-heading" class="text-lg font-semibold">Host</h2>
  <p
    class="mt-3 inline-flex items-center gap-2 rounded-md px-3 py-1 text-sm font-medium ring-1 {TONE_CLASSES[
      view.tone
    ]}"
    data-tone={view.tone}
  >
    <span aria-hidden="true" class="font-mono">{view.icon}</span>
    <span>{view.label}</span>
  </p>
  {#if view.message}
    <p class="mt-3 text-sm text-slate-700 dark:text-slate-300">{view.message}</p>
  {/if}
  {#if view.details.length > 0}
    <dl class="mt-4 grid grid-cols-1 gap-x-6 gap-y-2 text-sm sm:grid-cols-[max-content_1fr]">
      {#each view.details as d (d.term)}
        <dt class="text-slate-500 dark:text-slate-400">{d.term}</dt>
        <dd class="font-mono break-words">{d.value}</dd>
      {/each}
    </dl>
  {/if}
</section>
