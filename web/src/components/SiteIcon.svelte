<script>
  import { initials } from '../stores.svelte.js';
  // Shows the site's stored favicon, falling back to initials when it has none (or it fails).
  let { site, size = 42, radius = 11 } = $props();
  // Remember which URL failed, so a refreshed icon (new URL) is tried again.
  let failedUrl = $state(null);
  let src = $derived(site?.icon_url && site.icon_url !== failedUrl ? site.icon_url : null);
</script>

{#if src}
  <span class="site-icon has-img" style="width:{size}px;height:{size}px;border-radius:{radius}px;padding:{Math.round(size * 0.14)}px">
    <img {src} alt="" onerror={() => (failedUrl = src)} />
  </span>
{:else}
  <span class="site-icon" style="width:{size}px;height:{size}px;border-radius:{radius}px;font-size:{Math.round(size * 0.38)}px">{initials(site?.name)}</span>
{/if}
