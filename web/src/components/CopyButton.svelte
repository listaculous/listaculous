<script>
  import Copy from '@lucide/svelte/icons/copy';
  import Check from '@lucide/svelte/icons/check';
  let { text, label = 'Copy' } = $props();
  let copied = $state(false);
  async function copy() {
    try {
      await navigator.clipboard.writeText(text);
      copied = true;
      setTimeout(() => (copied = false), 1500);
    } catch {}
  }
</script>

<button type="button" class="secondary copy" onclick={copy} aria-live="polite">
  {#if copied}<Check size={14} /> Copied{:else}<Copy size={14} /> {label}{/if}
</button>
