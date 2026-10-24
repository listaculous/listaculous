<script>
  import ChevronRight from '@lucide/svelte/icons/chevron-right';
  import ArrowRight from '@lucide/svelte/icons/arrow-right';
  import Check from '@lucide/svelte/icons/check';
  import Wand from '@lucide/svelte/icons/wand-sparkles';
  import Settings from '@lucide/svelte/icons/settings-2';
  import { api } from '../api.js';
  import { go, route } from '../router.svelte.js';
  import { refreshNav } from '../stores.svelte.js';
  import SiteForm from './SiteForm.svelte';
  import AutoSetup from '../components/AutoSetup.svelte';
  import Alert from '../components/Alert.svelte';

  // Step 1 (#/new-site) creates the site, sending nothing yet; step 2 (#/new-site/<id>) picks its
  // email provider. Step 2 has its own URL so it survives a detour to Integrations.
  const siteId = route.parts[1];
  let site = $state(null);
  let error = $state('');
  let mode = $state('auto');
  let loading = $state(false);
  // While automatic setup runs, the site changes underneath us, so manual entry waits for it.
  // '' until AutoSetup's first check ('unknown' if it failed): a run may already be in progress.
  let setupStatus = $state('');
  let running = $derived(setupStatus === 'running');
  let manualBlocked = $derived(running || setupStatus === '');
  $effect(() => { if (running) mode = 'auto'; });

  // Only the newest reload may update the site, so a slow older response can't bring back
  // settings that setup has since replaced.
  let loadSeq = 0;
  async function load() {
    const n = ++loadSeq;
    loading = true;
    try {
      const s = await api(`/sites/${siteId}`);
      if (n === loadSeq) { site = s; error = ''; }
    } catch (err) {
      if (n === loadSeq) error = err.message;
    } finally {
      if (n === loadSeq) loading = false;
    }
  }
  if (siteId) load();

  const finish = () => go(`/sites/${siteId}`);
</script>

<div class="crumbs"><a href="#/">Dashboard</a><ChevronRight size={14} /><span>New site</span></div>
<div class="page-head">
  <div>
    <h1>Add a site</h1>
    <p class="sub">A site is one of your websites. It has its own sender identity, email provider and newsletters.</p>
  </div>
</div>

<ol class="wizard-steps" aria-label="Progress">
  <li class:done={siteId} class:current={!siteId} aria-current={siteId ? undefined : 'step'}>
    <span class="n">{#if siteId}<Check size={14} />{:else}1{/if}</span> Site details
  </li>
  <li class:current={siteId} aria-current={siteId ? 'step' : undefined}><span class="n">2</span> Email provider</li>
</ol>

{#if !siteId}
  <div class="card" style="max-width:860px">
    <SiteForm sections="details" submitLabel="Continue" onSaved={(s) => { refreshNav(); go(`/new-site/${s.id}`); }} />
  </div>
{:else if !site && error}
  <Alert kind="error">{error}</Alert>
{:else if site}
  <div style="max-width:860px">
    {#if error}<Alert kind="error">Couldn't refresh the site: {error} <button class="ghost" onclick={load}>Retry</button></Alert>{/if}
    <div class="choices" role="radiogroup" aria-label="How to set up sending">
      <label class="choice">
        <input type="radio" name="mode" value="auto" bind:group={mode} onchange={load} />
        <span class="ci"><Wand size={18} /></span>
        <div><strong>Amazon SES, set up automatically</strong><span>Recommended. Verifies the domain, adds DNS records and creates a send-only key.</span></div>
      </label>
      <label class="choice">
        <input type="radio" name="mode" value="manual" bind:group={mode} onchange={load} disabled={manualBlocked} />
        <span class="ci"><Settings size={18} /></span>
        <div><strong>Enter provider details</strong><span>{running ? 'Available once automatic setup finishes.' : setupStatus === '' ? 'Checking setup status…' : 'SES keys you already have, SMTP, or log only for testing.'}</span></div>
      </label>
    </div>

    <!-- Setup stays mounted (only hidden) in manual mode, so a running setup keeps reporting
         progress and the site below is reloaded when it finishes. -->
    <div hidden={mode !== 'auto'}>
      <AutoSetup {site} onStatus={(st) => (setupStatus = st)} returnTo={`/new-site/${siteId}`} onApplied={() => { load(); refreshNav(); }} />
      <div class="actions" style="justify-content:space-between">
        <span class="small muted">{site.provider_kind === 'ses' ? `${site.name} now sends with Amazon SES.` : 'Until setup runs, emails are only written to the server log.'}</span>
        <button onclick={finish}>{site.provider_kind === 'ses' ? 'Finish' : 'Skip for now'} <ArrowRight size={16} /></button>
      </div>
    </div>
    {#if mode === 'manual' && (loading || error)}
      <!-- Never offer the form with a site snapshot that failed to refresh. -->
      {#if loading}<div class="card muted small">Loading…</div>{/if}
    {:else if mode === 'manual'}
      <!-- Shown only once the latest reload finished, and re-created whenever the site reloads,
           so it never edits or saves stale provider settings. -->
      {#key site}
        <div class="card">
          <SiteForm {site} sections="provider" submitLabel="Save and finish" onSaved={() => { refreshNav(); finish(); }} />
        </div>
      {/key}
    {/if}
  </div>
{/if}
