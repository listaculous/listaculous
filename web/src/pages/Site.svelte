<script>
  import ChevronRight from '@lucide/svelte/icons/chevron-right';
  import Mail from '@lucide/svelte/icons/mail';
  import Plus from '@lucide/svelte/icons/plus';
  import Send from '@lucide/svelte/icons/send';
  import Trash2 from '@lucide/svelte/icons/trash-2';
  import ShieldCheck from '@lucide/svelte/icons/shield-check';
  import Users from '@lucide/svelte/icons/users';
  import { api, fmtDate } from '../api.js';
  import { go } from '../router.svelte.js';
  import { nav, refreshNav } from '../stores.svelte.js';
  import SiteIcon from '../components/SiteIcon.svelte';
  import AutoSetup from '../components/AutoSetup.svelte';
  import RefreshCw from '@lucide/svelte/icons/refresh-cw';
  import SiteForm from './SiteForm.svelte';
  import Alert from '../components/Alert.svelte';
  import Empty from '../components/Empty.svelte';
  import CodeBlock from '../components/CodeBlock.svelte';
  let { id } = $props();

  let site = $state(null);
  let lists = $state([]);
  let events = $state([]);
  let tab = $state('lists');
  let showNew = $state(false);
  let newList = $state({ name: '', description: '', double_opt_in: true });
  let testEmail = $state('');
  let msg = $state('');
  let error = $state('');

  async function load() {
    [site, lists, events] = await Promise.all([api(`/sites/${id}`), api(`/sites/${id}/lists`), api(`/sites/${id}/events`)]);
  }
  load();

  let counts = $derived(Object.fromEntries(nav.lists.map((l) => [l.id, l])));

  async function createList(e) {
    e.preventDefault();
    error = '';
    try {
      const l = await api(`/sites/${id}/lists`, { method: 'POST', body: newList });
      refreshNav();
      go(`/lists/${l.id}`);
    } catch (err) {
      error = err.message;
    }
  }

  async function sendTest(e) {
    e.preventDefault();
    msg = error = '';
    try {
      await api(`/sites/${id}/test`, { method: 'POST', body: { email: testEmail } });
      msg = `Test email sent to ${testEmail}.`;
    } catch (err) {
      error = err.message;
    }
  }

  let iconBusy = $state(false);
  async function refreshIcon() {
    iconBusy = true;
    try {
      site = await api(`/sites/${id}/icon/refresh`, { method: 'POST' });
      msg = site.icon_url ? 'Icon updated.' : `No icon found on ${site.domain}; showing initials.`;
      refreshNav();
    } catch (err) {
      error = err.message;
    } finally {
      iconBusy = false;
    }
  }

  async function remove() {
    if (!confirm(`Delete ${site.name}, all its newsletters, subscribers and campaigns? This cannot be undone.`)) return;
    await api(`/sites/${id}`, { method: 'DELETE' });
    refreshNav();
    go('/');
  }

  const setTab = (t) => { tab = t; msg = error = ''; };
</script>

{#if site}
  <div class="crumbs"><a href="#/">Dashboard</a><ChevronRight size={14} /><span>{site.name}</span></div>
  <div class="page-head">
    <div style="display:flex;gap:14px;align-items:center">
      <SiteIcon {site} size={52} radius={14} />
      <div>
        <h1>{site.name}</h1>
        <p class="sub">{site.domain} · {site.from_name} &lt;{site.from_email}&gt; <span class="badge">{site.provider_kind}</span></p>
      </div>
    </div>
    <div class="actions"><button onclick={() => { setTab('lists'); showNew = true; }}><Plus size={16} /> New newsletter</button></div>
  </div>

  <div class="tabs" role="tablist">
    <button class:active={tab === 'lists'} onclick={() => setTab('lists')}>Newsletters</button>
    <button class:active={tab === 'settings'} onclick={() => setTab('settings')}>Settings</button>
    <button class:active={tab === 'deliverability'} onclick={() => setTab('deliverability')}>Deliverability</button>
    <button class:active={tab === 'events'} onclick={() => setTab('events')}>Bounces & complaints {#if events.length}<span class="badge failed" style="margin-left:4px">{events.length}</span>{/if}</button>
  </div>

  {#if msg}<Alert kind="ok">{msg}</Alert>{/if}
  {#if error}<Alert kind="error">{error}</Alert>{/if}

  {#if tab === 'lists'}
    {#if showNew}
      <div class="card fade-in">
        <div class="card-head"><div><h2>New newsletter</h2><p class="muted small">Each newsletter has its own subscribers, signup form and campaigns.</p></div></div>
        <form onsubmit={createList}>
          <div class="grid-2">
            <label class="field">Name <input bind:value={newList.name} required placeholder="Weekly Digest" autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
            <label class="field">Description <span class="muted">(optional)</span> <input bind:value={newList.description} placeholder="The best of the week, every Friday" autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
          </div>
          <label class="check"><input type="checkbox" bind:checked={newList.double_opt_in} />
            <span>Require email confirmation (double opt-in)<span class="hint">Strongly recommended — protects your sender reputation and proves consent.</span></span></label>
          <div class="actions"><button><Plus size={16} /> Create newsletter</button><button type="button" class="ghost" onclick={() => (showNew = false)}>Cancel</button></div>
        </form>
      </div>
    {/if}

    {#if lists.length === 0 && !showNew}
      <div class="card">
        <Empty title="No newsletters yet" image>
          Create a newsletter for {site.name}, then paste its signup form on {site.domain}.
          {#snippet actions()}<button onclick={() => (showNew = true)}><Plus size={16} /> Create a newsletter</button>{/snippet}
        </Empty>
      </div>
    {:else}
      <div class="grid">
        {#each lists as l (l.id)}
          <a class="card site-card" href="#/lists/{l.id}">
            <div class="top">
              <div class="stat-icon" style="margin:0;width:42px;height:42px;border-radius:11px;display:grid;place-items:center;background:var(--accent-soft);color:var(--accent)"><Mail size={19} /></div>
              <div style="min-width:0"><h3>{l.name}</h3><div class="small muted">{l.description || (l.double_opt_in ? 'Double opt-in' : 'Single opt-in')}</div></div>
            </div>
            <div class="lists">
              <div class="li"><span style="display:flex;gap:6px;align-items:center"><Users size={14} /> Confirmed</span><strong>{counts[l.id]?.confirmed ?? 0}</strong></div>
              <div class="li"><span>Pending</span><span>{counts[l.id]?.pending ?? 0}</span></div>
            </div>
          </a>
        {/each}
      </div>
    {/if}
  {:else if tab === 'settings'}
    <div class="card"><SiteForm {site} onSaved={(s) => { site = s; refreshNav(); }} /></div>
    <div class="card">
      <div class="card-head"><div style="display:flex;gap:14px;align-items:center"><SiteIcon {site} size={40} radius={10} />
        <div><h2>Site icon</h2><p class="muted small">Fetched automatically from {site.domain}'s favicon. Re-fetch after changing it on your site.</p></div></div>
        <button class="secondary" onclick={refreshIcon} disabled={iconBusy}><RefreshCw size={15} class={iconBusy ? 'spin' : ''} /> Refresh icon</button></div>
    </div>
    <div class="card danger-zone">
      <div class="card-head"><div><h2>Delete site</h2><p class="muted small">Removes the site with all of its newsletters, subscribers and campaign history.</p></div>
        <button class="danger" onclick={remove}><Trash2 size={16} /> Delete site</button></div>
    </div>
  {:else if tab === 'deliverability'}
    <AutoSetup {site} onApplied={() => { load(); refreshNav(); }} />
    <div class="grid-2">
      <div class="card">
        <div class="card-head"><div><h2>Send a test email</h2><p class="muted small">Checks that {site.provider_kind.toUpperCase()} is configured correctly.</p></div></div>
        <form onsubmit={sendTest} class="actions" style="flex-wrap:nowrap">
          <input type="email" bind:value={testEmail} placeholder="you@example.com" required autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" />
          <button><Send size={16} /> Send</button>
        </form>
      </div>
      <div class="card">
        <div class="card-head"><div><h2>Built-in protections</h2></div></div>
        <div class="steps small" style="gap:8px">
          <div style="display:flex;gap:8px"><ShieldCheck size={16} color="var(--ok)" /> One-click unsubscribe headers (RFC 8058)</div>
          <div style="display:flex;gap:8px"><ShieldCheck size={16} color="var(--ok)" /> Unsubscribe link + mailing address in every footer</div>
          <div style="display:flex;gap:8px"><ShieldCheck size={16} color="var(--ok)" /> Automatic bounce & complaint suppression</div>
          <div style="display:flex;gap:8px"><ShieldCheck size={16} color="var(--ok)" /> Plain-text alternative and List-Id on every message</div>
        </div>
      </div>
    </div>
    <div class="card">
      <div class="card-head"><div><h2>Manual bounce & complaint setup (Amazon SES)</h2><p class="muted small">Not needed if you use Automatic setup above. To connect SES notifications by hand:</p></div></div>
      <ol class="steps">
        <li><div>In <strong>Amazon SNS</strong>, create a topic, e.g. <code>listaculous-{site.domain}</code>.</div></li>
        <li><div style="min-width:0">Add an <strong>HTTPS</strong> subscription pointing at this endpoint (it confirms automatically):
          <div style="margin-top:8px"><CodeBlock code={site.webhook_url} /></div></div></li>
        <li><div>In <strong>SES → Verified identities → {site.domain} → Notifications</strong>, send <em>Bounce</em> and <em>Complaint</em> feedback to that topic (or use a configuration set event destination).</div></li>
      </ol>
    </div>
  {:else}
    <div class="card flush">
      {#if events.length === 0}
        <Empty title="No bounces or complaints" icon={ShieldCheck}>Your list is healthy. Permanent bounces and spam complaints will appear here and be suppressed automatically.</Empty>
      {:else}
        <div class="table-wrap">
          <table>
            <thead><tr><th>Email</th><th>Type</th><th>Detail</th><th>When</th></tr></thead>
            <tbody>
              {#each events as e (e.id)}
                <tr><td class="cell-strong">{e.email}</td><td><span class="badge {e.kind}">{e.kind}</span></td><td class="small muted">{e.detail}</td><td class="small muted">{fmtDate(e.created_at)}</td></tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    </div>
  {/if}
{/if}
