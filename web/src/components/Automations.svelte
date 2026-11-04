<script>
  import Workflow from '@lucide/svelte/icons/workflow';
  import Plus from '@lucide/svelte/icons/plus';
  import { api } from '../api.js';
  import { go } from '../router.svelte.js';
  import Alert from './Alert.svelte';
  let { list } = $props();

  let items = $state(null);
  let error = $state('');
  let busy = $state(false);

  $effect(() => {
    api(`/lists/${list.id}/automations`).then((r) => (items = r)).catch((e) => (error = e.message));
  });

  const newId = () => Math.random().toString(36).slice(2, 10);

  // Starters: `emails` emails with a wait of `gap` days between them.
  const templates = [
    { key: 'blank', name: 'Start from scratch', desc: 'An empty flow; add steps on the canvas.', emails: 0 },
    { key: 'welcome', name: 'Welcome series', desc: 'Three emails over a week, starting right after someone joins.', emails: 3, gaps: [2, 4] },
    { key: 'course', name: 'Email course', desc: 'One lesson a day for 7 days.', emails: 7, gaps: [1, 1, 1, 1, 1, 1] },
  ];

  async function create(t) {
    error = '';
    busy = true;
    try {
      const a = await api(`/lists/${list.id}/automations`, { method: 'POST', body: { name: t.key === 'blank' ? 'New automation' : t.name } });
      if (t.emails) {
        const steps = [];
        for (let i = 0; i < t.emails; i++) {
          if (i > 0) steps.push({ id: newId(), type: 'wait', amount: t.gaps[i - 1], unit: 'days' });
          const c = await api(`/automations/${a.id}/emails`, { method: 'POST', body: { subject: '' } });
          steps.push({ id: newId(), type: 'email', campaign_id: c.id });
        }
        await api(`/automations/${a.id}`, { method: 'PUT', body: { steps } });
      }
      go(`/automations/${a.id}`);
    } catch (e) {
      error = e.message;
    } finally {
      busy = false;
    }
  }

  function triggerText(t) {
    return { subscribed: 'Joins the newsletter', tag_added: 'Gets a tag', clicked: 'Clicks a link', manual: 'Added by hand' }[t?.type] ?? '';
  }
  const count = (steps) => steps.reduce((n, s) => n + 1 + (s.type === 'branch' ? count(s.yes) + count(s.no) : 0), 0);
</script>

{#if error}<Alert kind="error">{error}</Alert>{/if}

{#if items && items.length}
  <div class="card flush">
    <div class="table-wrap">
      <table>
        <thead><tr><th>Automation</th><th>Status</th><th>Trigger</th><th>Steps</th><th>In progress</th><th>Completed</th></tr></thead>
        <tbody>
          {#each items as a (a.id)}
            <tr>
              <td class="cell-strong"><a href="#/automations/{a.id}" style="color:inherit">{a.name}</a></td>
              <td><span class="badge {a.status === 'active' ? 'sending' : a.status}">{a.status === 'active' ? 'running' : a.status}</span></td>
              <td class="small muted">{triggerText(a.trigger)}</td>
              <td class="small">{count(a.steps)}</td>
              <td class="small">{a.stats.in_progress.toLocaleString()}</td>
              <td class="small">{a.stats.completed.toLocaleString()}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  </div>
{/if}

{#if items}
  <div class="card">
    <div class="card-head"><div><h2>{items.length ? 'New automation' : 'Automate your emails'}</h2>
      <p class="muted small">Send emails, wait, branch on opens, clicks and tags, and tag or move people, all mapped out as a flowchart.</p></div></div>
    <div class="grid">
      {#each templates as t (t.key)}
        <button class="template-card" disabled={busy} onclick={() => create(t)}>
          <strong style="display:flex;gap:8px;align-items:center">{#if t.key === 'blank'}<Plus size={16} />{:else}<Workflow size={16} />{/if} {t.name}</strong>
          <span>{t.desc}</span>
        </button>
      {/each}
    </div>
  </div>
{/if}
