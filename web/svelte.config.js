// Svelte 5 compiles `<script lang="ts">` natively; no preprocessor needed.
// CSS is emitted to files (never injected inline), so the daemon's CSP can
// forbid inline styles.
export default {
  compilerOptions: { css: 'external' },
};
