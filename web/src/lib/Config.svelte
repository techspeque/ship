<script lang="ts">
  import type { ConfigSection } from './project';
  import { TONE_CLASSES } from './tone';
  import type { Load } from './view';

  let { config }: { config: Load<ConfigSection[]> } = $props();
</script>

<section
  aria-labelledby="config-heading"
  class="rounded-lg border border-slate-200 bg-white p-5 shadow-sm dark:border-slate-700 dark:bg-slate-900"
>
  <h2 id="config-heading" class="text-lg font-semibold">Config and secrets</h2>
  <p class="mt-1 text-sm text-slate-600 dark:text-slate-300">
    Secret values are never shown; set them with <code>ship secrets set</code>.
  </p>
  {#if config.kind === 'loading'}
    <p class="mt-3 text-sm">Loading config…</p>
  {:else if config.kind === 'error'}
    <p class="mt-3 rounded-md px-3 py-2 text-sm ring-1 {TONE_CLASSES.bad}">
      <span aria-hidden="true" class="font-mono">✕</span>
      {config.message}
    </p>
  {:else}
    {#each config.value as section (section.title)}
      <h3 class="mt-4 font-medium">{section.title}</h3>
      {#if section.vars.length === 0 && section.secrets.length === 0}
        <p class="text-sm text-slate-500 dark:text-slate-400">Nothing set.</p>
      {:else}
        <table class="mt-1 w-full text-left text-sm">
          <thead class="text-slate-500 dark:text-slate-400">
            <tr>
              <th scope="col" class="py-1 pr-4 font-normal">Name</th>
              <th scope="col" class="py-1 pr-4 font-normal">Value</th>
              <th scope="col" class="py-1 font-normal">Updated</th>
            </tr>
          </thead>
          <tbody>
            {#each section.vars as v (v.name)}
              <tr>
                <th scope="row" class="py-1 pr-4 font-mono font-normal">{v.name}</th>
                <td class="py-1 pr-4 font-mono break-all">{v.value}</td>
                <td class="py-1"><time datetime={v.updatedAt}>{v.updatedAt}</time></td>
              </tr>
            {/each}
            {#each section.secrets as s (s.name)}
              <tr>
                <th scope="row" class="py-1 pr-4 font-mono font-normal">{s.name}</th>
                <td class="py-1 pr-4 text-slate-500 italic dark:text-slate-400">secret, not shown</td>
                <td class="py-1"><time datetime={s.updatedAt}>{s.updatedAt}</time></td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    {/each}
  {/if}
</section>
