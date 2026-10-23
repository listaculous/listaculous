<script>
  import CopyButton from './CopyButton.svelte';
  let { code } = $props();
</script>

<div class="code-block">
  <pre class="code">{code}</pre>
  <CopyButton text={code} />
</div>
