<script lang="ts">
  import { href } from './route';
  import { FOCUS, TONE_CLASSES } from './tone';
  import type { ProjectsView } from './view';

  let { view }: { view: ProjectsView } = $props();
</script>

<section
  aria-labelledby="projects-heading"
  class="rounded-lg border border-slate-200 bg-white p-5 shadow-sm dark:border-slate-700 dark:bg-slate-900"
>
  <h2 id="projects-heading" class="text-lg font-semibold">Projects</h2>
  {#if view.kind === 'list'}
    <ul class="mt-3 divide-y divide-slate-200 dark:divide-slate-700">
      {#each view.rows as row (row.id)}
        <li class="flex flex-wrap items-baseline justify-between gap-2 py-2 text-sm">
          <a
            href={href({ page: 'project', project: row.slug })}
            class="rounded-sm font-medium text-sky-700 underline hover:text-sky-900 dark:text-sky-300 {FOCUS}"
            >{row.slug}</a
          >
          {#each row.environments as env (env.id)}
            <span class="text-slate-600 dark:text-slate-300">
              {env.name}: desired <span class="font-mono">{env.desired}</span>, observed
              <span class="font-mono">{env.status ?? 'no service'}</span>
            </span>
          {/each}
          <span class="text-slate-500 dark:text-slate-400">
            created <time datetime={row.created}>{row.created}</time>
          </span>
        </li>
      {/each}
    </ul>
  {:else if view.kind === 'error'}
    <p class="mt-3 rounded-md px-3 py-2 text-sm ring-1 {TONE_CLASSES.bad}">
      <span aria-hidden="true" class="font-mono">✕</span>
      {view.text}
    </p>
  {:else}
    <p class="mt-3 text-sm text-slate-600 dark:text-slate-300">{view.text}</p>
  {/if}
</section>
