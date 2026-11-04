<script>
  import { onDestroy } from 'svelte';
  import ChevronRight from '@lucide/svelte/icons/chevron-right';
  import Play from '@lucide/svelte/icons/play';
  import Pause from '@lucide/svelte/icons/pause';
  import Trash2 from '@lucide/svelte/icons/trash-2';
  import Zap from '@lucide/svelte/icons/zap';
  import SquarePen from '@lucide/svelte/icons/square-pen';
  import ArrowUp from '@lucide/svelte/icons/arrow-up';
  import ArrowDown from '@lucide/svelte/icons/arrow-down';
  import UserPlus from '@lucide/svelte/icons/user-plus';
  import X from '@lucide/svelte/icons/x';
  import Alert from '../components/Alert.svelte';
  import Flow from '../components/Flow.svelte';
  import ConditionEditor from '../components/ConditionEditor.svelte';
  import { api } from '../api.js';
  import { go } from '../router.svelte.js';
  let { id } = $props();

  let a = $state(null);
  let steps = $state([]);
  let trigger = $state({ type: 'subscribed' });
  let list = $state(null);
  let tags = $state([]);
  let lists = $state([]);
  let campaigns = $state([]);
  let error = $state('');
  let msg = $state('');
  let saving = $state(false);
  let enrollTarget = $state('all');
  let enrollTag = $state(0);
  let newTag = $state('');
  let poll;
  let destroyed = false;

  const newId = () => Math.random().toString(36).slice(2, 10);

  // Bumped whenever a save starts or ends: a poll that started before one may have read an older
  // flow and must not replace the editor with it.
  let gen = 0;
  async function load(first = false) {
    try {
      const started = gen;
      const fresh = await api(`/automations/${id}`);
      if (saving || started !== gen) return;
      a = fresh;
      // Copies: local edits must not change `a`, which is what's actually saved.
      // Don't clobber the flowchart while a save is pending; otherwise follow the server.
      if (first || !pending) { steps = structuredClone(fresh.steps); trigger = structuredClone(fresh.trigger); }
      if (first) {
        list = await api(`/lists/${a.list_id}`);
        [tags, lists, campaigns] = await Promise.all([
          api(`/sites/${list.site_id}/tags`),
          api(`/sites/${list.site_id}/lists`),
          api(`/lists/${a.list_id}/campaigns`),
        ]);
      }
    } catch (e) {
      error = e.message;
    } finally {
      clearTimeout(poll);
      if (!destroyed) poll = setTimeout(load, a?.status === 'active' ? 10000 : 30000);
    }
  }
  load(true);
  onDestroy(() => { destroyed = true; clearTimeout(poll); });

  // Every change saves right away; saves are serialized so an older one can't land last.
  let chain = Promise.resolve();
  let pending = 0;
  // `prepare` runs first, inside the queue (e.g. creating an email before inserting its step).
  function save(body, prepare) {
    pending++;
    gen++;
    chain = chain.then(async () => {
      saving = true;
      error = '';
      try {
        if (prepare) await prepare();
        a = await api(`/automations/${id}`, { method: 'PUT', body: body ?? { steps: $state.snapshot(steps), trigger: $state.snapshot(trigger) } });
        if (pending === 1) { steps = $state.snapshot(a.steps); trigger = $state.snapshot(a.trigger); }
        return true;
      } catch (e) {
        error = e.message;
        // Rejected: show what's actually saved again (unless more edits are queued behind it).
        if (pending === 1 && a) { steps = $state.snapshot(a.steps); trigger = $state.snapshot(a.trigger); }
        return false;
      } finally {
        gen++;
        saving = false;
        pending--;
      }
    });
    return chain;
  }

  let selected = $state('trigger');
  let adding = $state(null);

  function find(arr, sid, parent = null) {
    for (let i = 0; i < arr.length; i++) {
      const s = arr[i];
      if (s.id === sid) return { arr, i, s, parent };
      if (s.type === 'branch') {
        const r = find(s.yes, sid, s) || find(s.no, sid, s);
        if (r) return r;
      }
    }
    return null;
  }
  let sel = $derived(selected === 'trigger' ? null : find(steps, selected));
  let emails = $derived(a?.emails ?? {});
  // Campaigns people can have opened or clicked: this automation's emails, then the newsletter's.
  let campaignOptions = $derived([
    ...Object.values(emails).map((c) => ({ id: c.id, label: `Email: ${c.subject || 'Untitled email'}` })),
    ...campaigns.map((c) => ({ id: c.id, label: `Campaign: ${c.subject || 'Untitled campaign'}` })),
  ]);

  function defaultCondition() {
    const first = Object.values(emails)[0];
    if (first) return { type: 'opened', campaign_id: first.id };
    if (tags[0]) return { type: 'has_tag', tag_id: tags[0].id };
    return { type: 'on_list', list_id: a.list_id };
  }

  async function add(arr, i, type) {
    adding = null;
    let s = { id: newId(), type };
    if (type === 'email') {
      // Created and inserted inside the save queue, so no other save can go out (and drop the
      // new, not-yet-placed email) in between. While queued, polling leaves `steps` (and `arr`) alone.
      return save(undefined, async () => {
        const c = await api(`/automations/${id}/emails`, { method: 'POST', body: { subject: '' } });
        s.campaign_id = c.id;
        a.emails[c.id] = c;
        arr.splice(i, 0, s);
        selected = s.id;
      });
    }
    if (type === 'wait') Object.assign(s, { amount: 1, unit: 'days' });
    if (type === 'branch') Object.assign(s, { condition: defaultCondition(), yes: [], no: [] });
    if (type === 'tag') Object.assign(s, { action: 'add', tag_id: tags[0]?.id ?? 0 });
    if (type === 'list') Object.assign(s, { action: 'subscribe', list_id: lists.find((l) => l.id !== a.list_id)?.id ?? 0 });
    if (type === 'goal') Object.assign(s, { name: '', condition: defaultCondition(), unmet: 'continue' });
    arr.splice(i, 0, s);
    selected = s.id;
    await save();
  }

  function remove() {
    if (!sel) return;
    const inner = sel.s.type === 'branch' ? sel.s.yes.length + sel.s.no.length : 0;
    if (inner && !confirm(`Delete this if/else and the ${inner} step${inner === 1 ? '' : 's'} inside it?`)) return;
    if (sel.s.type === 'email' && emails[sel.s.campaign_id]?.subject && !confirm('Delete this email step and the email?')) return;
    sel.arr.splice(sel.i, 1);
    selected = 'trigger';
    save();
  }

  function move(d) {
    if (!sel) return;
    const j = sel.i + d;
    if (j < 0 || j >= sel.arr.length) return;
    const [s] = sel.arr.splice(sel.i, 1);
    sel.arr.splice(j, 0, s);
    save();
  }

  async function setStatus(status) {
    msg = '';
    if (await save({ status })) msg = status === 'active' ? 'Automation is running.' : 'Paused.';
  }

  async function rename(name) {
    if (name.trim() && name !== a.name) await save({ name });
  }

  async function del() {
    if (!confirm(`Delete "${a.name}" and its emails? People in it stop getting them.`)) return;
    await api(`/automations/${id}`, { method: 'DELETE' });
    go(`/lists/${a.list_id}/automations`);
  }

  async function enroll() {
    msg = error = '';
    try {
      const r = await api(`/automations/${id}/enroll`, { method: 'POST', body: { target: enrollTarget, tag_id: enrollTarget === 'tag' ? enrollTag : undefined } });
      msg = `Added ${r.enrolled.toLocaleString()} subscriber${r.enrolled === 1 ? '' : 's'}.${a.status === 'active' ? '' : ' They start once the automation is running.'}`;
      load();
    } catch (e) { error = e.message; }
  }

  async function createTag(assign) {
    if (!newTag.trim()) return;
    try {
      const t = await api(`/sites/${list.site_id}/tags`, { method: 'POST', body: { name: newTag } });
      if (!tags.some((x) => x.id === t.id)) tags = [...tags, { ...t, subscribers: 0 }].sort((x, y) => x.name.localeCompare(y.name));
      newTag = '';
      if (assign(t.id)) save();
    } catch (e) { error = e.message; }
  }

  // ---- Labels for the flowchart cards
  const tagName = (tid) => tags.find((t) => t.id === tid)?.name;
  const listName = (lid) => lists.find((l) => l.id === lid)?.name;
  const campaignName = (cid) => campaignOptions.find((c) => c.id === cid)?.label.replace(/^(Email|Campaign): /, '');
  const plural = (n, u) => `${n} ${n === 1 ? u.slice(0, -1) : u}`;
  function condText(c) {
    if (!c) return '';
    if (c.type === 'opened') return c.campaign_id ? `opened “${campaignName(c.campaign_id) ?? '?'}”` : 'opened (choose an email)';
    if (c.type === 'clicked') return c.campaign_id ? `clicked ${c.url ? 'a link' : 'any link'} in “${campaignName(c.campaign_id) ?? '?'}”` : 'clicked (choose an email)';
    if (c.type === 'has_tag') return c.tag_id ? `has tag “${tagName(c.tag_id) ?? '?'}”` : 'has tag (choose one)';
    if (c.type === 'on_list') return c.list_id ? `is on ${listName(c.list_id) ?? '?'}` : 'is on (choose a newsletter)';
    return '';
  }
  function triggerText(t) {
    if (!t) return '';
    if (t.type === 'subscribed') return `Joins ${list?.name ?? 'the newsletter'}${t.include_imports ? ' (including imports)' : ''}`;
    if (t.type === 'tag_added') return t.tag_id ? `Gets tag “${tagName(t.tag_id) ?? '?'}”` : 'Gets a tag (choose one)';
    if (t.type === 'clicked') return t.campaign_id ? `Clicks ${t.url ? 'a link' : 'any link'} in “${campaignName(t.campaign_id) ?? '?'}”` : 'Clicks a link (choose a campaign)';
    return 'Added by hand';
  }
  const ctx = {
    get selected() { return selected; },
    get adding() { return adding; },
    set adding(v) { adding = v; },
    get editable() { return !saving; },
    select: (sid) => { selected = sid; adding = null; },
    add,
    here: (s) => a?.at_step?.[s.id] ?? 0,
    problem: (s) => {
      if (s.type === 'email') { const c = emails[s.campaign_id]; return !c || !c.subject.trim() || !c.html_body.trim() || c.status === 'paused'; }
      if (s.type === 'tag') return !s.tag_id;
      if (s.type === 'list') return !s.list_id;
      const c = s.type === 'wait' ? s.until : s.condition;
      return !!c && !!condText(c).includes('choose');
    },
    title: (s) => {
      if (s.type === 'email') return emails[s.campaign_id]?.subject || 'Untitled email';
      if (s.type === 'wait') return s.until ? `Wait until ${condText(s.until)}` : `Wait ${plural(s.amount, s.unit)}`;
      if (s.type === 'branch') return `If ${condText(s.condition)}`;
      if (s.type === 'tag') return `${s.action === 'add' ? 'Add' : 'Remove'} tag ${s.tag_id ? `“${tagName(s.tag_id) ?? '?'}”` : '(choose one)'}`;
      if (s.type === 'list') return `${s.action === 'subscribe' ? 'Subscribe to' : 'Unsubscribe from'} ${s.list_id ? listName(s.list_id) ?? '?' : '(choose a newsletter)'}`;
      if (s.type === 'goal') return `Goal${s.name ? `: ${s.name}` : ''}`;
      return 'Exit';
    },
    detail: (s) => {
      if (s.type === 'email') {
        const c = emails[s.campaign_id];
        if (!c) return '';
        if (c.status === 'paused') return 'Paused after send failures';
        if (!c.subject.trim() || !c.html_body.trim()) return 'Not written yet';
        return `${c.stats.sent.toLocaleString()} sent · ${c.stats.opened.toLocaleString()} opened · ${c.stats.clicked.toLocaleString()} clicked`;
      }
      if (s.type === 'wait' && s.until) return s.amount ? `or ${plural(s.amount, s.unit)} at most` : 'no time limit';
      if (s.type === 'goal') return `When ${condText(s.condition)}`;
      if (s.type === 'list' && s.action === 'subscribe') return 'Skips double opt-in';
      return '';
    },
  };
</script>

{#if a}
  <div class="crumbs">
    <a href="#/">Dashboard</a><ChevronRight size={14} />
    {#if list}<a href="#/sites/{list.site_id}">{list.site.name}</a><ChevronRight size={14} /><a href="#/lists/{list.id}/automations">{list.name}</a><ChevronRight size={14} />{/if}
    <span>Automation</span>
  </div>
  <div class="page-head">
    <div>
      <input class="title-input" value={a.name} onchange={(e) => rename(e.currentTarget.value)} aria-label="Automation name" autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" />
      <p class="sub"><span class="badge {a.status === 'active' ? 'sending' : a.status}">{a.status === 'active' ? 'running' : a.status}</span>
        {a.stats.enrolled.toLocaleString()} enrolled · {a.stats.in_progress.toLocaleString()} in progress · {a.stats.completed.toLocaleString()} completed
        <span class="muted">{saving ? '· Saving…' : ''}</span></p>
    </div>
    <div class="actions">
      {#if a.status === 'active'}
        <button class="secondary" onclick={() => setStatus('paused')}><Pause size={16} /> Pause</button>
      {:else}
        <button class="primary-grad" onclick={() => setStatus('active')}><Play size={16} /> {a.status === 'draft' ? 'Start automation' : 'Resume'}</button>
      {/if}
    </div>
  </div>

  {#if msg}<Alert kind="ok">{msg}</Alert>{/if}
  {#if error}<Alert kind="error">{error}</Alert>{/if}
  {#if a.status === 'draft'}
    <Alert kind="info">Build the flow, write the emails, then start it. Only people who trigger it after it starts are enrolled{trigger.type === 'manual' ? ' (or whoever you add by hand)' : ''}.</Alert>
  {:else if a.status === 'paused'}
    <Alert kind="warn">Paused: nobody moves and no emails go out. People who trigger it meanwhile are enrolled when you resume.</Alert>
  {/if}

  <div class="flow-layout">
    <div class="card flow-canvas">
      <div class="flow">
        <button class="flow-node trigger" class:selected={selected === 'trigger'} onclick={() => ctx.select('trigger')}>
          <span class="flow-icon"><Zap size={16} /></span>
          <span class="flow-text"><span class="flow-kicker">Starts when someone</span><span class="flow-title">{triggerText(trigger)}</span></span>
        </button>
        <Flow {steps} path="root" {ctx} />
        {#if !steps.length || steps[steps.length - 1].type !== 'exit'}<div class="flow-end">End</div>{/if}
      </div>
    </div>

    <div class="card flow-panel">
      {#if selected === 'trigger'}
        <div class="card-head"><h2>Trigger</h2></div>
        <label class="field">Start when someone…
          <select value={trigger.type} onchange={(e) => {
            const type = e.currentTarget.value;
            trigger = type === 'tag_added' ? { type, tag_id: tags[0]?.id ?? 0 } : type === 'clicked' ? { type, campaign_id: campaigns[0]?.id ?? 0, url: '' } : { type };
            save();
          }}>
            <option value="subscribed">joins this newsletter</option>
            <option value="tag_added">gets a tag</option>
            <option value="clicked">clicks a link in a campaign</option>
            <option value="manual">is added by hand</option>
          </select></label>
        {#if trigger.type === 'subscribed'}
          <label class="check"><input type="checkbox" checked={trigger.include_imports} onchange={(e) => { trigger.include_imports = e.currentTarget.checked; save(); }} />
            <span>Include CSV imports<span class="hint">Off by default, so importing your existing readers doesn't send them all the first email.</span></span></label>
        {:else if trigger.type === 'tag_added'}
          <label class="field">Tag
            <select value={trigger.tag_id} onchange={(e) => { trigger.tag_id = +e.currentTarget.value; save(); }}>
              <option value={0}>Choose a tag…</option>
              {#each tags as t (t.id)}<option value={t.id}>{t.name}</option>{/each}
            </select></label>
          <div class="actions" style="flex-wrap:nowrap;margin-top:-6px">
            <input placeholder="New tag" bind:value={newTag} autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" />
            <button class="secondary" onclick={() => createTag((t) => { trigger.tag_id = t; return true; })}>Create</button>
          </div>
          <p class="hint">Tag people on the Subscribers tab, when importing a CSV, or with a Tag step in another automation.</p>
        {:else if trigger.type === 'clicked'}
          <label class="field">Campaign
            <select value={trigger.campaign_id} onchange={(e) => { trigger.campaign_id = +e.currentTarget.value; save(); }}>
              <option value={0}>Choose a campaign…</option>
              {#each campaigns as c (c.id)}<option value={c.id}>{c.subject || 'Untitled campaign'}</option>{/each}
            </select></label>
          <label class="field">Only this link <span class="muted">(optional)</span>
            <input type="url" value={trigger.url} placeholder="https://… (blank = any link)" onchange={(e) => { trigger.url = e.currentTarget.value.trim(); save(); }} autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
        {:else}
          <p class="small muted">Nobody joins on their own. Add people below.</p>
        {/if}
        <p class="hint">Each person goes through an automation once. Triggers only count from when it's first started.</p>

        <div class="panel-section">
          <h3>Add existing subscribers</h3>
          <div class="actions" style="flex-wrap:nowrap">
            <select bind:value={enrollTarget}><option value="all">Everyone confirmed</option><option value="tag">People with a tag</option></select>
            {#if enrollTarget === 'tag'}
              <select bind:value={enrollTag}><option value={0}>Choose…</option>{#each tags as t (t.id)}<option value={t.id}>{t.name}</option>{/each}</select>
            {/if}
          </div>
          <button class="secondary" style="margin-top:10px" onclick={enroll} disabled={!steps.length || (enrollTarget === 'tag' && !enrollTag)}><UserPlus size={15} /> Add to automation</button>
        </div>
      {:else if sel}
        {@const s = sel.s}
        <div class="card-head">
          <h2>{({ email: 'Send email', wait: 'Wait', branch: 'If/else', tag: 'Tag', list: 'Newsletter', goal: 'Goal', exit: 'Exit' })[s.type]}</h2>
          <div class="actions">
            <button class="ghost icon-btn" title="Move up" aria-label="Move up" disabled={sel.i === 0} onclick={() => move(-1)}><ArrowUp size={15} /></button>
            <button class="ghost icon-btn" title="Move down" aria-label="Move down" disabled={sel.i === sel.arr.length - 1} onclick={() => move(1)}><ArrowDown size={15} /></button>
            <button class="ghost icon-btn" title="Close" aria-label="Close" onclick={() => ctx.select('trigger')}><X size={15} /></button>
          </div>
        </div>

        {#if s.type === 'email'}
          {@const c = emails[s.campaign_id]}
          {#if c}
            <p class="cell-strong" style="margin:0 0 4px">{c.subject || 'Untitled email'}</p>
            <p class="small muted" style="margin:0 0 14px">{ctx.detail(s)}</p>
            <button onclick={() => go(`/campaigns/${c.id}`)}><SquarePen size={15} /> {c.subject ? 'Edit email' : 'Write email'}</button>
          {/if}
          <p class="hint">Sent the moment someone reaches this step, with the usual unsubscribe footer. Links are tracked for click conditions.</p>
        {:else if s.type === 'wait'}
          <div class="actions" style="flex-wrap:nowrap">
            <input type="number" min="0" max="5000" value={s.amount} style="max-width:110px" onchange={(e) => { s.amount = Math.max(0, +e.currentTarget.value || 0); save(); }} autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" />
            <select value={s.unit} onchange={(e) => { s.unit = e.currentTarget.value; save(); }}>
              <option value="minutes">minutes</option><option value="hours">hours</option><option value="days">days</option><option value="weeks">weeks</option>
            </select>
          </div>
          <label class="check" style="margin-top:14px"><input type="checkbox" checked={!!s.until} onchange={(e) => { s.until = e.currentTarget.checked ? defaultCondition() : undefined; if (!s.until) delete s.until; save(); }} />
            <span>…or until a condition is met<span class="hint">Moves on as soon as it's met; the time above is then the longest it waits (0 = no limit).</span></span></label>
          {#if s.until}<ConditionEditor cond={s.until} {tags} {lists} {campaignOptions} onchange={save} />{/if}
        {:else if s.type === 'branch'}
          <ConditionEditor cond={s.condition} {tags} {lists} {campaignOptions} onchange={save} />
          <p class="hint">Checked the moment someone gets here. Put a Wait before it to give people time to open or click. Both paths carry on with the steps below.</p>
        {:else if s.type === 'tag'}
          <div class="seg"><button class:active={s.action === 'add'} onclick={() => { s.action = 'add'; save(); }}>Add tag</button><button class:active={s.action === 'remove'} onclick={() => { s.action = 'remove'; save(); }}>Remove tag</button></div>
          <label class="field" style="margin-top:14px">Tag
            <select value={s.tag_id} onchange={(e) => { s.tag_id = +e.currentTarget.value; save(); }}>
              <option value={0}>Choose a tag…</option>
              {#each tags as t (t.id)}<option value={t.id}>{t.name}</option>{/each}
            </select></label>
          <div class="actions" style="flex-wrap:nowrap;margin-top:-6px">
            <input placeholder="New tag" bind:value={newTag} autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" />
            <button class="secondary" onclick={() => createTag((t) => {
              // Resolved after the request: the flowchart may have been reloaded meanwhile.
              const cur = find(steps, s.id)?.s;
              if (cur) cur.tag_id = t;
              return !!cur;
            })}>Create</button>
          </div>
        {:else if s.type === 'list'}
          <div class="seg"><button class:active={s.action === 'subscribe'} onclick={() => { s.action = 'subscribe'; save(); }}>Subscribe to</button><button class:active={s.action === 'unsubscribe'} onclick={() => { s.action = 'unsubscribe'; save(); }}>Unsubscribe from</button></div>
          <label class="field" style="margin-top:14px">Newsletter
            <select value={s.list_id} onchange={(e) => { s.list_id = +e.currentTarget.value; save(); }}>
              <option value={0}>Choose a newsletter…</option>
              {#each lists as l (l.id)}<option value={l.id}>{l.name}{l.id === a.list_id ? ' (this one)' : ''}</option>{/each}
            </select></label>
          <p class="hint">{s.action === 'subscribe'
            ? 'Adds them as confirmed without a confirmation email, so only use it where they agreed to it. People who unsubscribed from that newsletter are never re-added.'
            : 'Unsubscribing them from this newsletter also ends the automation for them. Subscribe to another and unsubscribe from this one to move them.'}</p>
        {:else if s.type === 'goal'}
          <label class="field">Name <input value={s.name} placeholder="e.g. Bought the course" onchange={(e) => { s.name = e.currentTarget.value; save(); }} autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
          <span class="small cell-strong">Goal is met when they…</span>
          <ConditionEditor cond={s.condition} {tags} {lists} {campaignOptions} onchange={save} />
          <label class="field">If someone gets here without meeting it
            <select value={s.unmet} onchange={(e) => { s.unmet = e.currentTarget.value; save(); }}>
              <option value="continue">Continue anyway</option><option value="wait">Wait here until they meet it</option><option value="exit">End the automation for them</option>
            </select></label>
          <p class="hint">Anyone earlier in the flow who meets the goal skips straight here, e.g. stop the sales emails once they buy.</p>
        {:else}
          <p class="small muted">Ends the automation for anyone who gets here.</p>
        {/if}
        <div class="panel-section"><button class="danger" onclick={remove}><Trash2 size={15} /> Delete step</button></div>
      {/if}
    </div>
  </div>

  <div class="card danger-zone" style="max-width:760px">
    <div class="card-head"><div><h2>Delete automation</h2><p class="muted small">Removes the flow and its emails. People in it stop getting them.</p></div>
      <button class="danger" onclick={del}><Trash2 size={16} /> Delete</button></div>
  </div>
{/if}

<style>
  .title-input { font-size: 26px; font-weight: 700; letter-spacing: -.02em; font-family: inherit; border: 0; background: transparent; padding: 0; color: inherit; width: 100%; }
  .title-input:focus { box-shadow: none; border-bottom: 2px solid var(--accent); border-radius: 0; }
  .flow-layout { display: grid; grid-template-columns: minmax(0, 1fr) 360px; gap: 16px; align-items: start; }
  @media (max-width: 1000px) { .flow-layout { grid-template-columns: 1fr; } }
  .flow-canvas { overflow-x: auto; background: var(--bg-sunken); }
  .flow-panel { position: sticky; top: 20px; }
  .flow { display: flex; flex-direction: column; align-items: center; min-width: max-content; padding: 8px 0 16px; }
  .panel-section { margin-top: 18px; padding-top: 16px; border-top: 1px solid var(--border); }
  .panel-section h3 { font-size: 14px; margin: 0 0 10px; }
</style>
