<script>
  import Globe from '@lucide/svelte/icons/globe';
  import RefreshCw from '@lucide/svelte/icons/refresh-cw';
  import { api } from '../api.js';
  import Alert from './Alert.svelte';
  import CodeBlock from './CodeBlock.svelte';
  let { site } = $props();

  let integrations = $state([]);
  let dnsId = $state('');
  let result = $state(null);
  let error = $state('');
  let busy = $state(false);
  const host = $derived(site.public_url.replace(/^https?:\/\//, ''));

  api('/integrations').then((all) => {
    integrations = all.filter((i) => i.kind !== 'aws');
    if (integrations.length) dnsId = String(integrations[0].id);
  }).catch(() => {});

  async function run(create) {
    error = '';
    busy = true;
    try {
      result = await api(`/sites/${site.id}/link-domain`, { method: 'POST', body: create && dnsId ? { dns_integration_id: Number(dnsId) } : {} });
    } catch (err) {
      error = err.message;
    } finally {
      busy = false;
    }
  }
  // Check once when the card opens, without touching DNS.
  $effect(() => { if (site.public_url) run(false); });
</script>

<div class="card">
  <div class="card-head"><div><h2><Globe size={18} style="vertical-align:-3px" /> Link domain</h2>
    <p class="muted small">Confirm and unsubscribe links use <strong>{host}</strong>. It needs a DNS record pointing at this server.</p></div></div>
  {#if result}
    {#if result.reachable}
      <Alert kind="ok"><strong>{host}</strong> is live: it resolves and serves Listaculous over HTTPS.</Alert>
    {:else}
      <Alert kind="warn">{result.detail}</Alert>
    {/if}
    {#if result.dns?.status === 'created'}<p class="small">Created the CNAME in {result.dns.zone}.</p>{/if}
    {#if result.dns?.status === 'exists'}<p class="small">The CNAME already exists in {result.dns.zone}.</p>{/if}
    {#if !result.reachable}
      <p class="small muted" style="margin-bottom:6px">Record to have:</p>
      <CodeBlock code={`${result.record.fqdn}  CNAME  ${result.record.content}`} />
    {/if}
  {/if}
  {#if error}<Alert kind="error">{error}</Alert>{/if}
  <div class="actions" style="margin-top:12px">
    {#if integrations.length}
      <select bind:value={dnsId} aria-label="DNS provider">
        {#each integrations as i (i.id)}<option value={String(i.id)}>{i.name}</option>{/each}
      </select>
      <button onclick={() => run(true)} disabled={busy}>Create DNS record</button>
    {:else}
      <span class="small muted">Connect Porkbun on the <a href="#/integrations">Integrations</a> page to create the record automatically.</span>
    {/if}
    <button class="secondary" onclick={() => run(false)} disabled={busy}><RefreshCw size={15} class={busy ? 'spin' : ''} /> Check again</button>
  </div>
</div>
