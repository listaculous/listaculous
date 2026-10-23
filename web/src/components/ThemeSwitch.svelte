<script>
  import Sun from '@lucide/svelte/icons/sun';
  import Monitor from '@lucide/svelte/icons/monitor';
  import Moon from '@lucide/svelte/icons/moon';
  import { theme, setTheme } from '../theme.svelte.js';

  const options = [
    { value: 'light', label: 'Light', icon: Sun },
    { value: 'system', label: 'System', icon: Monitor },
    { value: 'dark', label: 'Dark', icon: Moon },
  ];
</script>

<div class="theme-switch" role="radiogroup" aria-label="Color theme">
  {#each options as o (o.value)}
    <label title="{o.label} theme">
      <input type="radio" name="theme" value={o.value} checked={theme.mode === o.value} onchange={() => setTheme(o.value)} />
      <o.icon size={14} />
      <span>{o.label}</span>
    </label>
  {/each}
</div>
