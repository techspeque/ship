<script lang="ts">
  import type { Badge } from './project';
  import { TONE_CLASSES } from './tone';

  let { badge }: { badge: Badge } = $props();
</script>

<span
  class="inline-flex items-center gap-1 rounded-md px-2 py-0.5 font-mono text-xs ring-1 {TONE_CLASSES[
    badge.tone
  ]}"
  data-tone={badge.tone}
>
  <span aria-hidden="true">{badge.icon}</span>
  <span>{badge.text}</span>
</span>
