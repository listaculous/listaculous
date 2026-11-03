<script>
  import GraduationCap from '@lucide/svelte/icons/graduation-cap';
  import Plus from '@lucide/svelte/icons/plus';
  import Play from '@lucide/svelte/icons/play';
  import Pause from '@lucide/svelte/icons/pause';
  import Trash2 from '@lucide/svelte/icons/trash-2';
  import { api } from '../api.js';
  import { go } from '../router.svelte.js';
  import Alert from './Alert.svelte';
  import Empty from './Empty.svelte';
  let { list } = $props();

  let courses = $state(null);
  let error = $state('');
  let busy = $state(false);

  async function load() {
    try { courses = await api(`/lists/${list.id}/sequences`); } catch (e) { error = e.message; }
  }
  load();

  async function act(fn) {
    error = '';
    busy = true;
    try { await fn(); await load(); } catch (e) { error = e.message; } finally { busy = false; }
  }

  // Starts a course with `days` blank lessons on day 0 … days-1, ready to write.
  const create = (days) => act(async () => {
    const q = await api(`/lists/${list.id}/sequences`, { method: 'POST', body: { name: `${days}-day email course` } });
    for (let d = 0; d < days; d++) {
      await api(`/sequences/${q.id}/lessons`, { method: 'POST', body: { subject: '', delay_days: d } });
    }
  });
  const addLesson = (q) => act(async () => {
    const c = await api(`/sequences/${q.id}/lessons`, { method: 'POST', body: { subject: '' } });
    go(`/campaigns/${c.id}`);
  });
  const setStatus = (q, status) => act(() => api(`/sequences/${q.id}`, { method: 'PUT', body: { status } }));
  const rename = (q, name) => act(() => api(`/sequences/${q.id}`, { method: 'PUT', body: { name } }));
  const remove = (q) => {
    if (!confirm(`Delete "${q.name}" and all its lessons? Subscribers stop receiving it.`)) return;
    act(() => api(`/sequences/${q.id}`, { method: 'DELETE' }));
  };
  const written = (c) => c.subject.trim() && c.html_body.trim();
</script>

{#if error}<Alert kind="error">{error}</Alert>{/if}

{#if courses && courses.length === 0}
  <div class="card flush">
    <Empty title="No email course yet">
      Send each new subscriber a series of lessons, one a day after they confirm. Subscribers you already have, and CSV imports, aren't enrolled.
      {#snippet actions()}<button onclick={() => create(7)} disabled={busy}><GraduationCap size={16} /> Create a 7-day course</button>{/snippet}
    </Empty>
  </div>
{/if}

{#each courses ?? [] as q (q.id)}
  <div class="card">
    <div class="card-head">
      <div>
        <h2><input value={q.name} onchange={(e) => rename(q, e.currentTarget.value)} aria-label="Course name"
          style="font:inherit;border:0;background:transparent;padding:0;color:inherit;width:auto" autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></h2>
        <p class="muted small"><span class="badge {q.status === 'active' ? 'sending' : q.status}">{q.status}</span>
          {q.stats.enrolled.toLocaleString()} enrolled · {q.stats.in_progress.toLocaleString()} in progress · {q.stats.completed.toLocaleString()} completed</p>
      </div>
      <div class="actions">
        {#if q.status === 'active'}
          <button class="secondary" onclick={() => setStatus(q, 'paused')} disabled={busy}><Pause size={15} /> Pause</button>
        {:else}
          <button class="primary-grad" onclick={() => setStatus(q, 'active')} disabled={busy}><Play size={15} /> {q.status === 'draft' ? 'Start course' : 'Resume'}</button>
        {/if}
      </div>
    </div>
    {#if q.status === 'draft'}
      <p class="small muted">Write the lessons, then start the course. From then on, everyone who confirms gets lesson 1 right away and the rest on their day, at the time of day they signed up. Unwritten lessons are held until you write them.</p>
    {:else if q.status === 'paused'}
      <Alert kind="warn">Paused: no lessons go out. People who sign up meanwhile are enrolled when you resume.</Alert>
    {/if}
    <div class="table-wrap">
      <table>
        <thead><tr><th style="width:80px">Day</th><th>Lesson</th><th>Status</th><th>Sent</th></tr></thead>
        <tbody>
          {#each q.lessons as c (c.id)}
            <tr>
              <td class="small">{c.delay_days === 0 ? 'Day 0' : `Day ${c.delay_days}`}</td>
              <td class="cell-strong"><a href="#/campaigns/{c.id}" style="color:inherit">{c.subject || 'Untitled lesson'}</a>
                {#if !written(c)}<span class="tiny muted"> · not written yet</span>{/if}</td>
              <td>{#if c.status === 'paused'}<span class="badge paused">paused</span>{:else}<span class="small muted">{written(c) ? 'ready' : '—'}</span>{/if}</td>
              <td class="small">{c.stats.sent.toLocaleString()}{#if c.stats.failed} · <span style="color:var(--danger)">{c.stats.failed} failed</span>{/if}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
    {#if q.lessons.some((c) => c.status === 'paused')}
      <p class="small" style="color:var(--danger);margin:10px 0 0">A lesson paused after repeated send failures. Subscribers wait at that lesson; open it to resume sending.</p>
    {/if}
    <div class="actions" style="margin-top:14px;justify-content:space-between">
      <button class="secondary" onclick={() => addLesson(q)} disabled={busy}><Plus size={15} /> Add lesson</button>
      <button class="danger" onclick={() => remove(q)} disabled={busy}><Trash2 size={15} /> Delete course</button>
    </div>
  </div>
{/each}
