<script>
  import CircleCheck from '@lucide/svelte/icons/circle-check';
  import CircleAlert from '@lucide/svelte/icons/circle-alert';
  import Info from '@lucide/svelte/icons/info';
  import TriangleAlert from '@lucide/svelte/icons/triangle-alert';
  let { kind = 'info', children } = $props();
  const icons = { ok: CircleCheck, error: CircleAlert, info: Info, warn: TriangleAlert };
  let Icon = $derived(icons[kind] ?? Info);
</script>

<div class="alert {kind} fade-in" role={kind === 'error' ? 'alert' : 'status'}>
  <Icon size={16} />
  <div>{@render children()}</div>
</div>
