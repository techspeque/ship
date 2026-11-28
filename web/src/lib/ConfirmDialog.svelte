<script lang="ts">
  import type { Confirmation } from './actions';
  import { FOCUS } from './tone';

  // A modal `<dialog>`: focus moves into it and stays there, Escape
  // cancels, and the first control (Cancel) has focus, so Enter alone
  // never confirms a destructive action.
  let {
    confirmation,
    onconfirm,
    oncancel,
  }: {
    confirmation: Confirmation | null;
    onconfirm: () => void;
    oncancel: () => void;
  } = $props();

  let dialog: HTMLDialogElement | undefined = $state();

  $effect(() => {
    if (dialog === undefined) return;
    if (confirmation !== null && !dialog.open) dialog.showModal();
    else if (confirmation === null && dialog.open) dialog.close();
  });

  const BUTTON = `rounded-md px-3 py-1.5 text-sm font-medium ${FOCUS}`;
</script>

<dialog
  bind:this={dialog}
  aria-labelledby="confirm-title"
  onclose={oncancel}
  class="m-auto max-w-md rounded-lg border border-slate-300 bg-white p-5 text-slate-900 shadow-lg backdrop:bg-slate-900/50 dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100"
>
  {#if confirmation !== null}
    <h2 id="confirm-title" class="text-lg font-semibold">{confirmation.title}</h2>
    {#each confirmation.lines as line (line)}
      <p class="mt-2 text-sm">{line}</p>
    {/each}
    <div class="mt-5 flex justify-end gap-3">
      <button
        type="button"
        onclick={oncancel}
        class="{BUTTON} border border-slate-300 bg-white hover:bg-slate-100 dark:border-slate-600 dark:bg-slate-800 dark:hover:bg-slate-700"
      >
        Cancel
      </button>
      <button
        type="button"
        onclick={onconfirm}
        class="{BUTTON} bg-red-700 text-white hover:bg-red-800 dark:bg-red-600 dark:hover:bg-red-500"
      >
        {confirmation.confirm}
      </button>
    </div>
  {/if}
</dialog>
