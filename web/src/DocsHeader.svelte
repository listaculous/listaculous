<script>
  import ArrowLeft from '@lucide/svelte/icons/arrow-left';
  import FileJson from '@lucide/svelte/icons/file-braces';
  import ThemeSwitch from './components/ThemeSwitch.svelte';
</script>

<div class="docs-bar">
  <a class="brand" href="/#/"><img src="/brand/logo-256.png" alt="" /> Listaculous <span class="docs-tag">API</span></a>
  <span class="spacer"></span>
  <a class="docs-link" href="/api/openapi.json" target="_blank" rel="noopener"><FileJson size={15} /> <span>OpenAPI</span></a>
  <a class="docs-link" href="/#/"><ArrowLeft size={15} /> <span>Back to app</span></a>
  <div class="docs-theme"><ThemeSwitch /></div>
</div>
