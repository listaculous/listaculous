<script>
  import Globe from '@lucide/svelte/icons/globe';
  import Mails from '@lucide/svelte/icons/mails';
  import UserCheck from '@lucide/svelte/icons/user-check';
  import Hourglass from '@lucide/svelte/icons/hourglass';
  import Plus from '@lucide/svelte/icons/plus';
  import Send from '@lucide/svelte/icons/send';
  import ChevronRight from '@lucide/svelte/icons/chevron-right';
  import { api, fmtDate } from '../api.js';
  import { initials } from '../stores.svelte.js';
  import Empty from '../components/Empty.svelte';

  let sites = $state([]);
  let dash = $state({ lists: [], recent_campaigns: [] });
  let loaded = $state(false);

  async function load() {
    [sites, dash] = await Promise.all([api('/sites'), api('/dashboard')]);
    loaded = true;
  }
  load();

  let totals = $derived(
    dash.lists.reduce((a, l) => ({ confirmed: a.confirmed + l.confirmed, pending: a.pending + l.pending }), { confirmed: 0, pending: 0 }),
  );
  let listName = $derived(Object.fromEntries(dash.lists.map((l) => [l.id, `${l.site_name} · ${l.name}`])));
  const fmt = (n) => n.toLocaleString();
</script>

<div class="page-head">
  <div>
    <h1>Dashboard</h1>
    <p class="sub">Everything across your sites at a glance.</p>
  </div>
  <div class="actions"><a class="btn" href="#/new-site"><Plus size={16} /> New site</a></div>
</div>

{#if loaded && sites.length === 0}
  <div class="card">
    <Empty title="Let's get your first newsletter flying" image>
      Add a site (one of your domains), connect Amazon SES or SMTP, then create a newsletter and drop the signup form on your website.
      {#snippet actions()}<a class="btn" href="#/new-site"><Plus size={16} /> Add your first site</a>{/snippet}
    </Empty>
  </div>
{:else if loaded}
  <div class="grid" style="margin-bottom:20px">
    <div class="card stat"><div class="stat-icon"><Globe size={19} /></div><div class="stat-label">Sites</div><div class="stat-value">{sites.length}</div></div>
    <div class="card stat"><div class="stat-icon"><Mails size={19} /></div><div class="stat-label">Newsletters</div><div class="stat-value">{dash.lists.length}</div></div>
    <div class="card stat ok"><div class="stat-icon"><UserCheck size={19} /></div><div class="stat-label">Confirmed subscribers</div><div class="stat-value">{fmt(totals.confirmed)}</div></div>
    <div class="card stat warn"><div class="stat-icon"><Hourglass size={19} /></div><div class="stat-label">Awaiting confirmation</div><div class="stat-value">{fmt(totals.pending)}</div></div>
  </div>

  <div class="card-head"><h2>Your sites</h2></div>
  <div class="grid" style="margin-bottom:24px">
    {#each sites as s (s.id)}
      {@const lists = dash.lists.filter((l) => l.site_id === s.id)}
      <a class="card site-card" href="#/sites/{s.id}">
        <div class="top">
          <div class="site-icon">{initials(s.name)}</div>
          <div style="min-width:0">
            <h3>{s.name}</h3>
            <div class="small muted">{s.domain}</div>
          </div>
          <span class="spacer"></span>
          <span class="badge">{s.provider_kind}</span>
        </div>
        <div class="lists">
          {#each lists as l (l.id)}
            <div class="li"><span>{l.name}</span><span><strong>{fmt(l.confirmed)}</strong> subscribers</span></div>
          {:else}
            <div class="li muted">No newsletters yet</div>
          {/each}
        </div>
      </a>
    {/each}
  </div>

  <div class="card flush">
    <div class="card-head"><div><h2>Recent campaigns</h2></div></div>
    {#if dash.recent_campaigns.length}
      <div class="table-wrap" style="margin-top:12px">
        <table>
          <thead><tr><th>Subject</th><th>Newsletter</th><th>Status</th><th>Created</th><th></th></tr></thead>
          <tbody>
            {#each dash.recent_campaigns as c (c.id)}
              <tr>
                <td class="cell-strong"><a href="#/campaigns/{c.id}" style="color:inherit">{c.subject || 'Untitled campaign'}</a></td>
                <td class="muted">{listName[c.list_id] ?? ''}</td>
                <td><span class="badge {c.status}">{c.status}</span></td>
                <td class="small muted">{fmtDate(c.created_at)}</td>
                <td class="actions-cell"><a href="#/campaigns/{c.id}" aria-label="Open"><ChevronRight size={16} /></a></td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {:else}
      <Empty title="No campaigns yet" icon={Send}>Open a newsletter and create your first campaign.</Empty>
    {/if}
  </div>
{/if}
