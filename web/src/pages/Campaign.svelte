<script>
  import { onDestroy } from 'svelte';
  import ChevronRight from '@lucide/svelte/icons/chevron-right';
  import Send from '@lucide/svelte/icons/send';
  import Save from '@lucide/svelte/icons/save';
  import Pause from '@lucide/svelte/icons/pause';
  import Play from '@lucide/svelte/icons/play';
  import RotateCcw from '@lucide/svelte/icons/rotate-ccw';
  import Trash2 from '@lucide/svelte/icons/trash-2';
  import FlaskConical from '@lucide/svelte/icons/flask-conical';
  import Monitor from '@lucide/svelte/icons/monitor';
  import Smartphone from '@lucide/svelte/icons/smartphone';
  import Braces from '@lucide/svelte/icons/braces';
  import CircleCheck from '@lucide/svelte/icons/circle-check';
  import CircleX from '@lucide/svelte/icons/circle-x';
  import SkipForward from '@lucide/svelte/icons/skip-forward';
  import Users from '@lucide/svelte/icons/users';
  import MailOpen from '@lucide/svelte/icons/mail-open';
  import MousePointerClick from '@lucide/svelte/icons/mouse-pointer-click';
  import Alert from '../components/Alert.svelte';
  import { api, fmtDate } from '../api.js';
  import { go } from '../router.svelte.js';
  let { id } = $props();

  let c = $state(null);
  let list = $state(null);
  let f = $state({ subject: '', preheader: '', html_body: '', text_body: '' });
  let preview = $state({ html: '', text: '' });
  let view = $state('html');
  let testEmail = $state('');
  let msg = $state('');
  let error = $state('');
  // Edit revisions: `dirty` only clears if no edits happened while a save was in flight.
  let rev = $state(0);
  let savedRev = $state(0);
  let dirty = $derived(rev !== savedRev);
  let timer;
  let poll;
  let destroyed = false;
  let device = $state('desktop');
  let htmlArea = $state();

  // Bumped by every edit and save: a poll that started before one must not overwrite the form
  // (or `c`) with what it read, since that may predate the save.
  let gen = 0;
  async function load() {
    try {
      const started = gen;
      const fresh = await api(`/campaigns/${id}`);
      list ??= await api(`/lists/${fresh.list_id}`);
      if (started !== gen || saving) return;
      c = fresh;
      if (!dirty) f = { subject: c.subject, preheader: c.preheader, html_body: c.html_body, text_body: c.text_body };
    } catch (err) {
      error = `Couldn't refresh campaign: ${err.message}`;
    } finally {
      // Keep polling while sending (also after a transient error) until the page is left.
      clearTimeout(poll);
      // Live automation emails refresh more slowly (counts change and the sender may pause one).
      if ((!c || c.status === 'sending') && !destroyed) poll = setTimeout(load, !c ? 5000 : c.automation_id ? 10000 : 2000);
    }
  }
  load().then(refreshPreview);
  onDestroy(() => { destroyed = true; clearTimeout(timer); clearTimeout(poll); });

  // Automation emails are always editable; the next send uses whatever is saved.
  let lesson = $derived(!!c?.automation_id);
  let editable = $derived(c?.status === 'draft' || lesson);

  function changed() {
    gen++;
    rev++;
    clearTimeout(timer);
    timer = setTimeout(refreshPreview, 400);
  }

  let previewReq = 0;
  async function refreshPreview() {
    const n = ++previewReq;
    try {
      const p = await api(`/campaigns/${id}/preview`, { method: 'POST', body: f });
      if (n === previewReq) preview = p;
    } catch {}
  }

  // Saves are serialized so an older PUT can never land after a newer one.
  let saveChain = Promise.resolve(true);
  let saving = $state(false);
  function save() {
    saveChain = saveChain.then(doSave, doSave);
    return saveChain;
  }

  async function doSave() {
    msg = error = '';
    saving = true;
    try {
      const submitted = rev;
      gen++;
      c = await api(`/campaigns/${id}`, { method: 'PUT', body: $state.snapshot(f) });
      gen++;
      savedRev = submitted;
      msg = 'Saved.';
      return true;
    } catch (err) {
      error = err.message;
      return false;
    } finally {
      saving = false;
    }
  }

  // Saves until the latest revision is persisted (edits may arrive during a save).
  async function saveLatest() {
    for (let i = 0; i < 5 && dirty; i++) if (!(await save())) return false;
    return !dirty;
  }

  async function test() {
    if (!(await saveLatest())) return;
    msg = error = '';
    try {
      await api(`/campaigns/${id}/test`, { method: 'POST', body: { email: testEmail } });
      msg = `Test sent to ${testEmail}.`;
    } catch (err) { error = err.message; }
  }

  async function send() {
    if (!(await saveLatest())) return;
    const n = list ? `the ${list.name} newsletter` : 'this newsletter';
    if (!confirm(`Send "${f.subject}" to every confirmed subscriber of ${n}? This cannot be undone.`)) return;
    msg = error = '';
    try {
      c = await api(`/campaigns/${id}/send`, { method: 'POST' });
      load();
    } catch (err) { error = err.message; }
  }

  async function retry() {
    msg = error = '';
    try {
      c = await api(`/campaigns/${id}/send`, { method: 'POST' });
      load();
    } catch (err) { error = err.message; }
  }

  // Saves pending edits first, so the queue resumes with the content on screen.
  async function resumeLesson() {
    if (!(await saveLatest())) return;
    await retry();
  }

  async function pause() {
    c = await api(`/campaigns/${id}/pause`, { method: 'POST' });
  }

  async function remove() {
    if (!confirm('Delete this campaign?')) return;
    await api(`/campaigns/${id}`, { method: 'DELETE' });
    go(`/lists/${c.list_id}/campaigns`);
  }

  const placeholders = ['{{name}}', '{{email}}', '{{list_name}}', '{{site_name}}', '{{unsubscribe_url}}'];
  function insert(token) {
    if (!editable || !htmlArea) return;
    const { selectionStart: a, selectionEnd: b } = htmlArea;
    f.html_body = f.html_body.slice(0, a) + token + f.html_body.slice(b);
    changed();
    queueMicrotask(() => { htmlArea.focus(); htmlArea.selectionStart = htmlArea.selectionEnd = a + token.length; });
  }

  let done = $derived(c ? c.stats.sent + c.stats.failed + c.stats.skipped : 0);
  let pct = $derived(c?.stats.total ? Math.round((100 * done) / c.stats.total) : 0);
</script>

{#if c}
  <div class="crumbs">
    <a href="#/">Dashboard</a><ChevronRight size={14} />
    {#if list}<a href="#/sites/{list.site_id}">{list.site.name}</a><ChevronRight size={14} /><a href="#/lists/{list.id}/{lesson ? 'automations' : 'campaigns'}">{list.name}</a><ChevronRight size={14} />{/if}
    {#if lesson}<a href="#/automations/{c.automation_id}">Automation</a><ChevronRight size={14} /><span>Email</span>{:else}<span>Campaign</span>{/if}
  </div>
  <div class="page-head">
    <div>
      <h1>{f.subject || (lesson ? 'Untitled email' : 'Untitled campaign')}</h1>
      <p class="sub">{#if lesson}<span class="badge">Automation email</span>{#if c.status === 'paused'} <span class="badge paused">paused</span>{/if}{:else}<span class="badge {c.status}">{c.status}</span>{/if}
        {#if editable}{dirty ? (saving ? 'Saving…' : 'Unsaved changes') : 'All changes saved'}{:else}Started {fmtDate(c.started_at)}{/if}</p>
    </div>
    <div class="actions">
      {#if lesson}
        <a class="btn secondary" href="#/automations/{c.automation_id}">Back to automation</a>
        <button class="secondary" onclick={save} disabled={!dirty || saving}><Save size={16} /> Save email</button>
        {#if c.status === 'paused'}<button onclick={resumeLesson}><Play size={16} /> Resume sending</button>{/if}
      {:else if editable}
        <button class="secondary" onclick={save} disabled={!dirty || saving}><Save size={16} /> Save draft</button>
        <button class="primary-grad" onclick={send}><Send size={16} /> Send now…</button>
      {:else if c.status === 'sending'}
        <button class="secondary" onclick={pause}><Pause size={16} /> Pause</button>
      {:else if c.status === 'paused'}
        <button onclick={send}><Play size={16} /> Resume sending</button>
      {:else if c.status === 'sent' && c.stats.failed > 0}
        <button class="secondary" onclick={retry}><RotateCcw size={16} /> Retry failed</button>
      {/if}
    </div>
  </div>

  {#if msg}<Alert kind="ok">{msg}</Alert>{/if}
  {#if error}<Alert kind="error">{error}</Alert>{/if}

  {#if lesson}
    <div class="card">
      <div class="card-head"><h2>Delivery</h2><span class="small muted">Sent automatically when someone reaches this step of the automation</span></div>
      <div class="grid">
        <div class="stat ok"><div class="stat-icon"><CircleCheck size={18} /></div><div class="stat-label">Sent</div><div class="stat-value">{c.stats.sent.toLocaleString()}</div></div>
        <div class="stat"><div class="stat-icon"><MailOpen size={18} /></div><div class="stat-label">Opened</div><div class="stat-value">{c.stats.opened.toLocaleString()}</div></div>
        <div class="stat"><div class="stat-icon"><MousePointerClick size={18} /></div><div class="stat-label">Clicked</div><div class="stat-value">{c.stats.clicked.toLocaleString()}</div></div>
        <div class="stat"><div class="stat-icon"><Users size={18} /></div><div class="stat-label">Queued</div><div class="stat-value">{(c.stats.total - c.stats.sent - c.stats.failed - c.stats.skipped).toLocaleString()}</div></div>
        <div class="stat danger"><div class="stat-icon"><CircleX size={18} /></div><div class="stat-label">Failed</div><div class="stat-value">{c.stats.failed.toLocaleString()}</div></div>
        <div class="stat warn"><div class="stat-icon"><SkipForward size={18} /></div><div class="stat-label">Skipped</div><div class="stat-value">{c.stats.skipped.toLocaleString()}</div></div>
      </div>
      {#if c.status === 'paused'}
        <p class="small" style="color:var(--danger);margin:14px 0 0">Paused after repeated send failures; people wait at this step. Fix the provider, then resume.</p>
      {/if}
    </div>
  {:else if c.status !== 'draft'}
    <div class="card">
      <div class="card-head"><h2>Delivery</h2><span class="small muted">{pct}% complete{#if c.finished_at} · finished {fmtDate(c.finished_at)}{/if}</span></div>
      <div class="progress" style="height:10px;margin-bottom:18px"><div style="width:{pct}%"></div></div>
      <div class="grid">
        <div class="stat"><div class="stat-icon"><Users size={18} /></div><div class="stat-label">Recipients</div><div class="stat-value">{c.stats.total.toLocaleString()}</div></div>
        <div class="stat ok"><div class="stat-icon"><CircleCheck size={18} /></div><div class="stat-label">Sent</div><div class="stat-value">{c.stats.sent.toLocaleString()}</div></div>
        <div class="stat"><div class="stat-icon"><MailOpen size={18} /></div><div class="stat-label">Opened</div><div class="stat-value">{c.stats.opened.toLocaleString()}</div></div>
        <div class="stat"><div class="stat-icon"><MousePointerClick size={18} /></div><div class="stat-label">Clicked</div><div class="stat-value">{c.stats.clicked.toLocaleString()}</div></div>
        <div class="stat danger"><div class="stat-icon"><CircleX size={18} /></div><div class="stat-label">Failed</div><div class="stat-value">{c.stats.failed.toLocaleString()}</div></div>
        <div class="stat warn"><div class="stat-icon"><SkipForward size={18} /></div><div class="stat-label">Skipped</div><div class="stat-value">{c.stats.skipped.toLocaleString()}</div></div>
      </div>
      <p class="small muted" style="margin:14px 0 0">Opens are approximate (some mail apps load images automatically, others block them); clicks count as opens.</p>
      {#if c.status === 'sent' && c.stats.failed > 0}
        <p class="small muted" style="margin:14px 0 0">Recipients whose delivery is unknown (e.g. a provider timeout) are never retried automatically, to avoid sending duplicates.</p>
      {/if}
    </div>
  {/if}

  <div class="split">
    <div class="card">
      <label class="field">Subject <input bind:value={f.subject} oninput={changed} disabled={!editable} placeholder="What's new this week" autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
      <label class="field">Preheader <span class="muted">(inbox preview text)</span>
        <input bind:value={f.preheader} oninput={changed} disabled={!editable} placeholder="A one-line teaser shown after the subject" autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
      <label class="field">HTML body
        <textarea bind:this={htmlArea} rows="18" bind:value={f.html_body} oninput={changed} disabled={!editable}></textarea>
      </label>
      {#if editable}
        <div class="kbd-row" style="margin:-6px 0 16px;align-items:center">
          <span class="small muted" style="display:inline-flex;gap:4px;align-items:center"><Braces size={14} /> Insert:</span>
          {#each placeholders as p (p)}<button type="button" class="secondary" style="padding:3px 8px;font-size:12px;font-family:var(--mono)" onclick={() => insert(p)}>{p}</button>{/each}
        </div>
      {/if}
      <label class="field">Plain-text body <span class="muted">(optional — generated from HTML if blank)</span>
        <textarea rows="5" bind:value={f.text_body} oninput={changed} disabled={!editable}></textarea>
      </label>
      <p class="hint" style="margin:0">An unsubscribe footer and your mailing address are always added automatically.</p>
      {#if editable && !lesson}
        <div style="margin-top:18px;padding-top:16px;border-top:1px solid var(--border)">
          <button class="danger" onclick={remove}><Trash2 size={15} /> Delete draft</button>
        </div>
      {/if}
    </div>

    <div class="card" style="position:sticky;top:20px">
      <div class="card-head">
        <div class="seg">
          <button class:active={view === 'html'} onclick={() => (view = 'html')}>HTML</button>
          <button class:active={view === 'text'} onclick={() => (view = 'text')}>Plain text</button>
        </div>
        {#if view === 'html'}
          <div class="seg">
            <button class:active={device === 'desktop'} onclick={() => (device = 'desktop')} aria-label="Desktop preview"><Monitor size={14} /></button>
            <button class:active={device === 'mobile'} onclick={() => (device = 'mobile')} aria-label="Mobile preview"><Smartphone size={14} /></button>
          </div>
        {/if}
      </div>
      {#if view === 'html'}
        <iframe class="preview-frame" title="Email preview" sandbox="" srcdoc={preview.html}
          style={device === 'mobile' ? 'max-width:390px;margin:0 auto' : ''}></iframe>
      {:else}
        <pre class="code" style="min-height:400px">{preview.text}</pre>
      {/if}
      <form class="actions" style="margin-top:14px;flex-wrap:nowrap" onsubmit={(e) => { e.preventDefault(); test(); }}>
        <input type="email" bind:value={testEmail} placeholder="Send a test to you@example.com" required autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" />
        <button class="secondary"><FlaskConical size={15} /> Test</button>
      </form>
    </div>
  </div>

  {#if !editable && c.status !== 'sending' && c.status !== 'paused'}
    <button class="danger" onclick={remove}><Trash2 size={15} /> Delete campaign</button>
  {/if}
{/if}
