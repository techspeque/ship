<script lang="ts">
  import { FOCUS } from './tone';

  let { refreshing, onrefresh }: { refreshing: boolean; onrefresh: () => void } = $props();
</script>

<button
  type="button"
  onclick={onrefresh}
  disabled={refreshing}
  aria-busy={refreshing}
  class="rounded-md border border-slate-300 bg-white px-3 py-1.5 text-sm font-medium hover:bg-slate-100 disabled:opacity-60 dark:border-slate-600 dark:bg-slate-800 dark:hover:bg-slate-700 {FOCUS}"
>
  {refreshing ? 'Refreshing…' : 'Refresh'}
</button>
