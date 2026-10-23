<script>
  import ChevronRight from '@lucide/svelte/icons/chevron-right';
  import Search from '@lucide/svelte/icons/search';
  import Download from '@lucide/svelte/icons/download';
  import Upload from '@lucide/svelte/icons/upload';
  import UserPlus from '@lucide/svelte/icons/user-plus';
  import Users from '@lucide/svelte/icons/users';
  import Plus from '@lucide/svelte/icons/plus';
  import Send from '@lucide/svelte/icons/send';
  import Save from '@lucide/svelte/icons/save';
  import Trash2 from '@lucide/svelte/icons/trash-2';
  import ExternalLink from '@lucide/svelte/icons/external-link';
  import UserCheck from '@lucide/svelte/icons/user-check';
  import Hourglass from '@lucide/svelte/icons/hourglass';
  import UserX from '@lucide/svelte/icons/user-x';
  import { api, fmtDate } from '../api.js';
  import { go } from '../router.svelte.js';
  import { nav, refreshNav } from '../stores.svelte.js';
  import Alert from '../components/Alert.svelte';
  import Empty from '../components/Empty.svelte';
  import CodeBlock from '../components/CodeBlock.svelte';
  let { id, tab = 'subscribers' } = $props();

  let list = $state(null);
  let subs = $state({ items: [], total: 0, page: 0, page_size: 50 });
  let campaigns = $state([]);
  let q = $state('');
  let status = $state('');
  let page = $state(0);
  let addEmail = $state('');
  let addName = $state('');
  let csv = $state('');
  let msg = $state('');
  let error = $state('');
  let edit = $state(null);
  let formStyle = $state('html');

  async function load() {
    list = await api(`/lists/${id}`);
    edit = { name: list.name, description: list.description, double_opt_in: !!list.double_opt_in, redirect_url: list.redirect_url };
    if (tab === 'subscribers') await loadSubs();
    if (tab === 'campaigns') campaigns = await api(`/lists/${id}/campaigns`);
  }
  let subsReq = 0;
  async function loadSubs() {
    // Only the latest request may update the table (typing fires many requests).
    const n = ++subsReq;
    const r = await api(`/lists/${id}/subscribers?q=${encodeURIComponent(q)}&status=${status}&page=${page}`);
    if (n === subsReq) subs = r;
  }
  load();

  let counts = $derived(nav.lists.find((l) => l.id === id) ?? { confirmed: 0, pending: 0, unsubscribed: 0 });

  const setTab = (t) => go(`/lists/${id}/${t}`);
  const flash = (m) => { msg = m; error = ''; };
  const fail = (e) => { error = e.message; msg = ''; };

  async function add(e) {
    e.preventDefault();
    try {
      const r = await api(`/lists/${id}/subscribers`, { method: 'POST', body: { email: addEmail, name: addName } });
      flash({ added: `${addEmail} added.`, already_subscribed: 'Already subscribed.', suppressed: 'That address is suppressed (bounced or complained) and cannot be added.' }[r.result]);
      addEmail = addName = '';
      loadSubs();
      refreshNav();
    } catch (err) { fail(err); }
  }

  async function importCsv() {
    try {
      const r = await api(`/lists/${id}/import`, { method: 'POST', body: { csv } });
      flash(`Imported ${r.added}. Skipped ${r.skipped} (already subscribed, unsubscribed or suppressed) and ${r.invalid} invalid.`);
      csv = '';
      loadSubs();
      refreshNav();
    } catch (err) { fail(err); }
  }

  async function onFile(e) {
    const f = e.target.files[0];
    if (f) csv = await f.text();
  }

  async function unsub(s, suppress = false) {
    if (suppress && !confirm(`Suppress ${s.email}? They will be unsubscribed from every newsletter on this site and can't be re-added.`)) return;
    await api(`/subscriptions/${s.id}/unsubscribe`, { method: 'POST', body: { suppress } });
    loadSubs();
    refreshNav();
  }

  async function del(s) {
    if (!confirm(`Permanently delete ${s.email} from this newsletter? (Unsubscribing keeps a record of consent instead.)`)) return;
    await api(`/subscriptions/${s.id}`, { method: 'DELETE' });
    loadSubs();
    refreshNav();
  }

  async function newCampaign() {
    const c = await api(`/lists/${id}/campaigns`, {
      method: 'POST',
      body: { subject: '', html_body: '<h1>Hello {{name}}!</h1>\n<p>Write your newsletter here.</p>' },
    });
    go(`/campaigns/${c.id}`);
  }

  async function saveList(e) {
    e.preventDefault();
    try {
      list = { ...list, ...(await api(`/lists/${id}`, { method: 'PUT', body: edit })) };
      flash('Saved.');
      refreshNav();
    } catch (err) { fail(err); }
  }

  async function removeList() {
    if (!confirm(`Delete ${list.name} with all its subscribers and campaigns?`)) return;
    await api(`/lists/${id}`, { method: 'DELETE' });
    refreshNav();
    go(`/sites/${list.site_id}`);
  }

  let embed = $derived(list ? `<form action="${list.subscribe_url}" method="post">
  <input type="email" name="email" placeholder="you@example.com" required>
  <input type="text" name="name" placeholder="Your name (optional)">
  <!-- spam trap: keep hidden -->
  <input type="text" name="website" tabindex="-1" autocomplete="off"
         style="position:absolute;left:-9999px" aria-hidden="true">
  <button type="submit">Subscribe</button>
</form>` : '');

  let embedJs = $derived(list ? `const res = await fetch("${list.subscribe_url}", {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({ email, name }),
});
const { message, error } = await res.json();` : '');
</script>

{#if list}
  <div class="crumbs"><a href="#/">Dashboard</a><ChevronRight size={14} /><a href="#/sites/{list.site_id}">{list.site.name}</a><ChevronRight size={14} /><span>{list.name}</span></div>
  <div class="page-head">
    <div>
      <h1>{list.name}</h1>
      <p class="sub">{list.description || `Newsletter for ${list.site.domain}`} <span class="badge">{list.double_opt_in ? 'double opt-in' : 'single opt-in'}</span></p>
    </div>
    <div class="actions"><button class="primary-grad" onclick={newCampaign}><Send size={16} /> New campaign</button></div>
  </div>

  <div class="grid" style="margin-bottom:20px">
    <div class="card stat ok" style="margin:0"><div class="stat-icon"><UserCheck size={19} /></div><div class="stat-label">Confirmed</div><div class="stat-value">{counts.confirmed.toLocaleString()}</div></div>
    <div class="card stat warn" style="margin:0"><div class="stat-icon"><Hourglass size={19} /></div><div class="stat-label">Pending</div><div class="stat-value">{counts.pending.toLocaleString()}</div></div>
    <div class="card stat danger" style="margin:0"><div class="stat-icon"><UserX size={19} /></div><div class="stat-label">Unsubscribed</div><div class="stat-value">{counts.unsubscribed.toLocaleString()}</div></div>
  </div>

  <div class="tabs" role="tablist">
    <button class:active={tab === 'subscribers'} onclick={() => setTab('subscribers')}>Subscribers</button>
    <button class:active={tab === 'campaigns'} onclick={() => setTab('campaigns')}>Campaigns</button>
    <button class:active={tab === 'form'} onclick={() => setTab('form')}>Signup form</button>
    <button class:active={tab === 'settings'} onclick={() => setTab('settings')}>Settings</button>
  </div>

  {#if msg}<Alert kind="ok">{msg}</Alert>{/if}
  {#if error}<Alert kind="error">{error}</Alert>{/if}

  {#if tab === 'subscribers'}
    <div class="card flush">
      <div class="toolbar">
        <div class="input-icon"><Search size={16} /><input placeholder="Search email or name" bind:value={q} oninput={() => { page = 0; loadSubs(); }} /></div>
        <select bind:value={status} onchange={() => { page = 0; loadSubs(); }}>
          <option value="">All statuses</option>
          <option value="confirmed">Confirmed</option>
          <option value="pending">Pending</option>
          <option value="unsubscribed">Unsubscribed</option>
        </select>
        <span class="spacer"></span>
        <span class="small muted">{subs.total.toLocaleString()} subscriber{subs.total === 1 ? '' : 's'}</span>
        <a class="btn secondary" href="/api/lists/{id}/export"><Download size={15} /> Export CSV</a>
      </div>
      {#if subs.items.length === 0}
        <Empty title={q || status ? 'No matches' : 'No subscribers yet'} icon={Users}>
          {q || status ? 'Try a different search or filter.' : 'Share your signup form, add someone below, or import a CSV.'}
        </Empty>
      {:else}
        <div class="table-wrap">
          <table>
            <thead><tr><th>Subscriber</th><th>Status</th><th>Source</th><th>Joined</th><th></th></tr></thead>
            <tbody>
              {#each subs.items as s (s.id)}
                <tr>
                  <td><div class="cell-strong">{s.email}</div>{#if s.name}<div class="small muted">{s.name}</div>{/if}</td>
                  <td>
                    <span class="badge {s.status}">{s.status}</span>
                    {#if s.subscriber_status !== 'active'}<span class="badge {s.subscriber_status}">{s.subscriber_status}</span>{/if}
                  </td>
                  <td class="small muted">{s.source}</td>
                  <td class="small muted">{fmtDate(s.created_at)}</td>
                  <td class="actions-cell small">
                    <span class="row-actions">
                      {#if s.status !== 'unsubscribed'}<button class="link" onclick={() => unsub(s)}>Unsubscribe</button>{/if}
                      {#if s.subscriber_status === 'active'}<button class="link" onclick={() => unsub(s, true)}>Suppress</button>{/if}
                      <button class="link danger-text" onclick={() => del(s)}>Delete</button>
                    </span>
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
        {#if subs.total > subs.page_size}
          <div class="pager">
            <button class="secondary" disabled={page === 0} onclick={() => { page--; loadSubs(); }}>Previous</button>
            <span class="small muted">Page {page + 1} of {Math.ceil(subs.total / subs.page_size)}</span>
            <button class="secondary" disabled={(page + 1) * subs.page_size >= subs.total} onclick={() => { page++; loadSubs(); }}>Next</button>
          </div>
        {/if}
      {/if}
    </div>

    <div class="split">
      <div class="card">
        <div class="card-head"><div><h2>Add subscriber</h2><p class="muted small">Added as confirmed — only add people who gave you permission.</p></div></div>
        <form onsubmit={add}>
          <label class="field">Email <input type="email" bind:value={addEmail} required placeholder="jane@example.com" /></label>
          <label class="field">Name <span class="muted">(optional)</span> <input bind:value={addName} placeholder="Jane" /></label>
          <button><UserPlus size={16} /> Add subscriber</button>
        </form>
      </div>
      <div class="card">
        <div class="card-head"><div><h2>Import CSV</h2><p class="muted small">Columns <code>email,name</code>, or a file exported from Listaculous. Only import people who opted in.</p></div></div>
        <label class="field">CSV file <input type="file" accept=".csv,text/csv,text/plain" onchange={onFile} /></label>
        <label class="field">…or paste <textarea rows="4" bind:value={csv} placeholder="jane@example.com,Jane"></textarea></label>
        <button onclick={importCsv} disabled={!csv.trim()}><Upload size={16} /> Import</button>
      </div>
    </div>
  {:else if tab === 'campaigns'}
    <div class="card flush">
      {#if campaigns.length === 0}
        <Empty title="No campaigns yet" image>
          Write your first email to {counts.confirmed.toLocaleString()} confirmed subscriber{counts.confirmed === 1 ? '' : 's'}.
          {#snippet actions()}<button onclick={newCampaign}><Plus size={16} /> New campaign</button>{/snippet}
        </Empty>
      {:else}
        <div class="table-wrap">
          <table>
            <thead><tr><th>Subject</th><th>Status</th><th style="width:220px">Delivery</th><th>Created</th></tr></thead>
            <tbody>
              {#each campaigns as c (c.id)}
                <tr>
                  <td class="cell-strong"><a href="#/campaigns/{c.id}" style="color:inherit">{c.subject || 'Untitled campaign'}</a></td>
                  <td><span class="badge {c.status}">{c.status}</span></td>
                  <td>
                    {#if c.stats.total}
                      <div class="progress"><div style="width:{(100 * (c.stats.sent + c.stats.failed + c.stats.skipped)) / c.stats.total}%"></div></div>
                      <div class="tiny muted" style="margin-top:4px">{c.stats.sent.toLocaleString()} / {c.stats.total.toLocaleString()} sent{#if c.stats.failed} · <span style="color:var(--danger)">{c.stats.failed} failed</span>{/if}</div>
                    {:else}<span class="small muted">—</span>{/if}
                  </td>
                  <td class="small muted">{fmtDate(c.created_at)}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    </div>
  {:else if tab === 'form'}
    <div class="card">
      <div class="card-head">
        <div><h2>Add the signup form to {list.site.domain}</h2><p class="muted small">Paste it anywhere and style it to match your site. The hidden <code>website</code> field is a spam trap.</p></div>
        <div class="seg">
          <button class:active={formStyle === 'html'} onclick={() => (formStyle = 'html')}>HTML form</button>
          <button class:active={formStyle === 'js'} onclick={() => (formStyle = 'js')}>JavaScript</button>
        </div>
      </div>
      <CodeBlock code={formStyle === 'html' ? embed : embedJs} />
      <p class="small muted" style="margin:14px 0 0">Endpoint <code>{list.subscribe_url}</code> accepts form posts or JSON <code>{'{ email, name }'}</code>, and is rate-limited per IP.
        {#if list.double_opt_in}New subscribers get a confirmation email first.{/if}</p>
    </div>
    <div class="card">
      <div class="card-head"><div><h2>Live preview</h2><p class="muted small">This form really subscribes — use your own address to try the flow.</p></div></div>
      <form action={list.subscribe_url} method="post" target="_blank" class="actions" style="max-width:560px;flex-wrap:nowrap">
        <input type="email" name="email" placeholder="you@example.com" required />
        <input type="text" name="website" tabindex="-1" autocomplete="off" style="position:absolute;left:-9999px" aria-hidden="true" />
        <button><ExternalLink size={15} /> Subscribe</button>
      </form>
    </div>
  {:else if tab === 'settings' && edit}
    <div class="card" style="max-width:760px">
      <form onsubmit={saveList}>
        <label class="field">Name <input bind:value={edit.name} required /></label>
        <label class="field">Description <input bind:value={edit.description} /></label>
        <label class="field">Redirect after signup <span class="muted">(optional)</span>
          <input type="url" bind:value={edit.redirect_url} placeholder="https://{list.site.domain}/thanks" />
          <span class="hint">Where plain HTML form submissions land. Leave blank for the built-in thank-you page.</span></label>
        <label class="check"><input type="checkbox" bind:checked={edit.double_opt_in} />
          <span>Require email confirmation (double opt-in)<span class="hint">Strongly recommended for deliverability and consent records.</span></span></label>
        <button><Save size={16} /> Save</button>
      </form>
    </div>
    <div class="card danger-zone" style="max-width:760px">
      <div class="card-head"><div><h2>Delete newsletter</h2><p class="muted small">Removes all subscribers and campaigns for this newsletter.</p></div>
        <button class="danger" onclick={removeList}><Trash2 size={16} /> Delete</button></div>
    </div>
  {/if}
{/if}
