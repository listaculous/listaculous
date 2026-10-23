<script>
  import { onDestroy } from 'svelte';
  import Wand from '@lucide/svelte/icons/wand-sparkles';
  import Eye from '@lucide/svelte/icons/eye';
  import Play from '@lucide/svelte/icons/play';
  import RefreshCw from '@lucide/svelte/icons/refresh-cw';
  import CircleCheck from '@lucide/svelte/icons/circle-check';
  import CircleDashed from '@lucide/svelte/icons/circle-dashed';
  import CircleX from '@lucide/svelte/icons/circle-x';
  import LoaderCircle from '@lucide/svelte/icons/loader-circle';
  import CircleMinus from '@lucide/svelte/icons/circle-minus';
  import Send from '@lucide/svelte/icons/send';
  import { api, fmtDate } from '../api.js';
  import Alert from './Alert.svelte';

  let { site, onApplied } = $props();

  let integrations = $state([]);
  let awsId = $state(null);
  let dnsId = $state('');
  let region = $state('');
  let plan = $state(null);
  let setup = $state({ status: 'not_started' });
  let busy = $state(false);
  let error = $state('');
  let draft = $state(null);
  let paMsg = $state('');
  let poll;
  let destroyed = false;
  onDestroy(() => { destroyed = true; clearTimeout(poll); });

  const body = () => ({ aws_integration_id: Number(awsId), dns_integration_id: dnsId ? Number(dnsId) : null, region: region || null });

  async function init() {
    integrations = await api('/integrations');
    const firstAws = integrations.find((i) => i.kind === 'aws');
    const firstDns = integrations.find((i) => i.kind !== 'aws');
    awsId = firstAws?.id ?? null;
    dnsId = firstDns ? String(firstDns.id) : '';
    region = firstAws?.summary.region ?? '';
    await refresh();
  }
  init();

  async function refresh() {
    clearTimeout(poll);
    try {
      const q = awsId ? `?aws_integration_id=${awsId}` : '';
      setup = await api(`/sites/${site.id}/setup${q}`);
    } catch (err) {
      error = err.message;
    }
    if (destroyed) return;
    // Fast polling while running; slower while DNS verification is pending.
    const pending = setup.status === 'applied' && setup.verification && setup.verification.dkim !== 'SUCCESS';
    if (setup.status === 'running') poll = setTimeout(refresh, 1500);
    else if (pending) poll = setTimeout(refresh, 20000);
  }

  // The plan shown is always for the exact selections it was requested with, and apply submits
  // those same selections (not whatever the inputs say now).
  let planReq = null;
  let previewSeq = 0;
  async function preview() {
    const req = body();
    const n = ++previewSeq;
    busy = true;
    error = '';
    try {
      const p = await api(`/sites/${site.id}/setup/plan`, { method: 'POST', body: req });
      if (n === previewSeq) { plan = p; planReq = req; }
    } catch (err) {
      if (n === previewSeq) error = err.message;
    } finally {
      if (n === previewSeq) busy = false;
    }
  }

  function selectionChanged() {
    previewSeq++; // discard any in-flight preview
    plan = null;
    planReq = null;
    busy = false;
  }

  async function applyNow() {
    const n = plan.records.filter((r) => r.action === 'create').length;
    const dnsText = plan.dns === 'automatic' ? `add ${n} DNS record(s) to ${plan.zone}` : 'leave DNS to you';
    if (!confirm(`Set up ${plan.identity} in Amazon SES (${plan.region}), ${dnsText}, and switch ${site.name} to a new send-only key?`)) return;
    busy = true;
    error = '';
    try {
      setup = await api(`/sites/${site.id}/setup/apply`, { method: 'POST', body: { ...planReq, confirm: true } });
      refresh();
    } catch (err) {
      error = err.message;
    } finally {
      busy = false;
    }
  }

  // When a run finishes: tell the parent (the site now sends via SES) and re-plan, so the records
  // shown include the real DKIM values (needed when adding DNS by hand).
  // Only on a running → finished transition (not when the page opens on a finished run).
  let lastStatus = null;
  $effect(() => {
    const status = setup.status;
    if (lastStatus === 'running' && (status === 'applied' || status === 'error')) {
      if (status === 'applied') onApplied?.();
      if (plan && planReq) preview();
    }
    lastStatus = status;
  });

  async function loadDraft() {
    draft = await api(`/sites/${site.id}/setup/production-access`);
    draft.contact_emails = draft.contact_emails.join(', ');
  }

  async function submitProduction(e) {
    e.preventDefault();
    if (!confirm('Submit this production access request to AWS? It applies to your whole AWS account in this region.')) return;
    busy = true;
    error = paMsg = '';
    try {
      const r = await api(`/sites/${site.id}/setup/production-access`, {
        method: 'POST',
        body: {
          aws_integration_id: Number(awsId),
          region: plan?.region || region || null,
          website_url: draft.website_url,
          use_case_description: draft.use_case_description,
          mail_type: draft.mail_type,
          contact_emails: draft.contact_emails.split(',').map((s) => s.trim()).filter(Boolean),
        },
      });
      paMsg = `Request submitted (status: ${r.review_status ?? 'pending'}). AWS usually replies within 24 hours.`;
      draft = null;
      preview();
    } catch (err) {
      error = err.message;
    } finally {
      busy = false;
    }
  }

  const stepOrder = ['feedback', 'identity', 'mail_from', 'dns', 'sender'];
  const stepNames = { feedback: 'Bounce & complaint feedback', identity: 'SES identity + DKIM', mail_from: 'Custom MAIL FROM', dns: 'DNS records', sender: 'Send-only IAM key' };
  const icon = (s) => ({ done: CircleCheck, running: LoaderCircle, error: CircleX, skipped: CircleMinus, manual: CircleMinus }[s] ?? CircleDashed);
  const verified = (s) => s === 'SUCCESS';
  let hasAws = $derived(integrations.some((i) => i.kind === 'aws'));
</script>

<div class="card">
  <div class="card-head">
    <div style="display:flex;gap:12px;align-items:center">
      <div class="stat-icon" style="margin:0;background:var(--gradient);color:#fff"><Wand size={19} /></div>
      <div><h2>Automatic setup</h2><p class="muted small">Sets up Amazon SES sending for {site.from_email.split('@')[1]}: DKIM, SPF, DMARC, bounce handling and a send-only key.</p></div>
    </div>
  </div>

  {#if error}<Alert kind="error">{error}</Alert>{/if}

  {#if !hasAws}
    <Alert kind="info">Connect an AWS account (and optionally Porkbun) on the <a href="#/integrations">Integrations</a> page to enable automatic setup.</Alert>
  {:else}
    <div class="grid" style="grid-template-columns:repeat(auto-fit,minmax(180px,1fr));margin-bottom:4px">
      <label class="field">AWS account
        <select bind:value={awsId} onchange={selectionChanged} disabled={busy}>
          {#each integrations.filter((i) => i.kind === 'aws') as i (i.id)}<option value={i.id}>{i.name}</option>{/each}
        </select>
      </label>
      <label class="field">DNS
        <select bind:value={dnsId} onchange={selectionChanged} disabled={busy}>
          {#each integrations.filter((i) => i.kind !== 'aws') as i (i.id)}<option value={String(i.id)}>{i.name}: add records automatically</option>{/each}
          <option value="">I'll add DNS records myself</option>
        </select>
      </label>
      <label class="field">SES region <input bind:value={region} oninput={selectionChanged} disabled={busy} placeholder="us-east-1" autocomplete="off" data-bwignore /></label>
    </div>

    {#if setup.status === 'running' || setup.status === 'applied' || setup.status === 'error'}
      <div class="setup-steps">
        {#each stepOrder as id (id)}
          {@const s = setup.state?.steps?.[id]}
          {@const Icon = icon(s?.status)}
          <div class="setup-step {s?.status ?? 'pending'}">
            <Icon size={17} class={s?.status === 'running' ? 'spin' : ''} />
            <div><div class="cell-strong">{stepNames[id]}</div>{#if s?.detail}<div class="small muted">{s.detail}</div>{/if}</div>
          </div>
        {/each}
      </div>
      {#if setup.status === 'error'}<Alert kind="error">Setup stopped: {setup.state.error}. Fix the cause and run it again. Steps already done are skipped.</Alert>{/if}
      {#if setup.verification}
        <div class="actions" style="margin:4px 0 16px">
          <span class="badge {verified(setup.verification.dkim) ? 'confirmed' : 'pending'}">DKIM {setup.verification.dkim?.toLowerCase()}</span>
          <span class="badge {verified(setup.verification.mail_from) ? 'confirmed' : 'pending'}">MAIL FROM {setup.verification.mail_from?.toLowerCase()}</span>
          <span class="badge {setup.verification.verified_for_sending ? 'confirmed' : 'pending'}">{setup.verification.verified_for_sending ? 'ready to send' : 'waiting for DNS'}</span>
          <button class="ghost" onclick={refresh}><RefreshCw size={14} /> Check now</button>
        </div>
        {#if !verified(setup.verification.dkim)}<p class="small muted" style="margin-top:-8px">DNS changes usually verify within minutes, occasionally a few hours. This page checks automatically.</p>{/if}
      {/if}
    {/if}

    {#if plan}
      <div class="card flush" style="margin:8px 0 16px;box-shadow:none">
        <div class="toolbar" style="justify-content:space-between">
          <div class="small"><strong>Plan</strong> for {plan.identity} · {plan.region} · DNS {plan.dns}</div>
          {#if plan.account.sandbox}<span class="badge pending">SES sandbox{plan.account.review_status ? ` · review ${plan.account.review_status.toLowerCase()}` : ''}</span>{:else}<span class="badge confirmed">production access</span>{/if}
        </div>
        <div class="table-wrap">
          <table>
            <tbody>
              {#each plan.steps as s (s.id)}
                <tr><td style="width:110px"><span class="badge {s.action === 'exists' ? 'confirmed' : s.action === 'conflict' ? 'failed' : s.action === 'skip' ? 'draft' : 'sending'}">{s.action}</span></td>
                  <td>{s.title}{#if s.note}<div class="small muted">{s.note}</div>{/if}</td></tr>
              {/each}
            </tbody>
          </table>
        </div>
        <div class="table-wrap">
          <table>
            <thead><tr><th>DNS record</th><th>Type</th><th>Value</th><th></th></tr></thead>
            <tbody>
              {#each plan.records as r, i (i)}
                <tr>
                  <td><code style="word-break:break-all">{r.fqdn ?? r.name}</code></td>
                  <td>{r.type}</td>
                  <td><code style="word-break:break-all">{r.priority ? `${r.priority} ` : ''}{r.content}</code>{#if r.note}<div class="small muted">{r.note}</div>{/if}</td>
                  <td><span class="badge {r.action === 'exists' ? 'confirmed' : r.action === 'conflict' ? 'failed' : r.action === 'manual' ? 'pending' : 'sending'}">{r.action}</span></td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      </div>
      {#if !plan.can_apply}<Alert kind="error">Some DNS records conflict with existing ones. Listaculous won't change existing records; update or remove them in Porkbun, then preview again.</Alert>{/if}
      {#if plan.dns === 'manual'}<Alert kind="info">Add the records above at your DNS host. DKIM values appear here after setup creates the SES identity.</Alert>{/if}
    {/if}

    <div class="actions">
      <button class="secondary" onclick={preview} disabled={busy || !awsId}><Eye size={16} /> {plan ? 'Refresh preview' : 'Preview changes'}</button>
      {#if plan}<button class="primary-grad" onclick={applyNow} disabled={busy || !plan.can_apply || setup.status === 'running'}><Play size={16} /> {setup.status === 'applied' || setup.status === 'error' ? 'Run setup again' : 'Apply setup'}</button>{/if}
    </div>
    {#if setup.state?.finished_at}<p class="tiny muted" style="margin:10px 0 0">Last run finished {fmtDate(setup.state.finished_at)}.</p>{/if}

    {#if plan?.account.sandbox}
      <div style="border-top:1px solid var(--border);margin-top:18px;padding-top:16px">
        <h3 style="margin-bottom:4px">Leave the SES sandbox</h3>
        <p class="small muted">In the sandbox you can send only {plan.account.max_24_hour_send ?? 200} emails a day, and only to verified addresses. Request production access once setup is done. AWS reviews it, usually within a day.</p>
        {#if paMsg}<Alert kind="ok">{paMsg}</Alert>{/if}
        {#if draft}
          <form onsubmit={submitProduction} style="margin-top:12px">
            <div class="grid-2">
              <label class="field">Website <input type="url" bind:value={draft.website_url} required autocomplete="off" data-bwignore /></label>
              <label class="field">Mail type
                <select bind:value={draft.mail_type}><option value="MARKETING">Marketing (newsletters)</option><option value="TRANSACTIONAL">Transactional</option></select>
              </label>
            </div>
            <label class="field">Contact emails <span class="muted">(comma separated)</span> <input bind:value={draft.contact_emails} autocomplete="off" data-bwignore /></label>
            <label class="field">How you send. Edit freely; AWS reads this.
              <textarea rows="12" bind:value={draft.use_case_description} required minlength="50" style="font-family:var(--font)"></textarea>
            </label>
            <div class="actions"><button disabled={busy}><Send size={16} /> Submit request to AWS</button><button type="button" class="ghost" onclick={() => (draft = null)}>Cancel</button></div>
          </form>
        {:else}
          <button class="secondary" onclick={loadDraft}><Send size={15} /> Draft a production access request</button>
        {/if}
      </div>
    {/if}
  {/if}
</div>
