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
  import CodeBlock from './CodeBlock.svelte';

  // `returnTo`: route the Integrations page links back to (for flows that send the user there).
  let { site, onApplied, onStatus, returnTo = '' } = $props();

  let integrations = $state([]);
  let awsId = $state(null);
  let dnsId = $state('');
  let region = $state('');
  let plan = $state(null);
  let setup = $state({ status: 'not_started' });
  let applying = $state(false); // an Apply request is in flight
  let statusKnown = $state(false); // false until the server has reported this site's setup status
  let busy = $state(false);
  let error = $state('');
  let draft = $state(null);
  let paMsg = $state('');
  let poll;
  let destroyed = false;
  onDestroy(() => { destroyed = true; clearTimeout(poll); });

  // Regions where SES (API v2) is available.
  const SES_REGIONS = [
    ['us-east-1', 'US East (N. Virginia)'], ['us-east-2', 'US East (Ohio)'], ['us-west-1', 'US West (N. California)'],
    ['us-west-2', 'US West (Oregon)'], ['ca-central-1', 'Canada (Central)'], ['ca-west-1', 'Canada West (Calgary)'],
    ['sa-east-1', 'South America (São Paulo)'], ['eu-west-1', 'Europe (Ireland)'], ['eu-west-2', 'Europe (London)'],
    ['eu-west-3', 'Europe (Paris)'], ['eu-central-1', 'Europe (Frankfurt)'], ['eu-central-2', 'Europe (Zurich)'],
    ['eu-south-1', 'Europe (Milan)'], ['eu-north-1', 'Europe (Stockholm)'], ['il-central-1', 'Israel (Tel Aviv)'],
    ['me-south-1', 'Middle East (Bahrain)'], ['me-central-1', 'Middle East (UAE)'], ['af-south-1', 'Africa (Cape Town)'],
    ['ap-south-1', 'Asia Pacific (Mumbai)'], ['ap-south-2', 'Asia Pacific (Hyderabad)'], ['ap-northeast-1', 'Asia Pacific (Tokyo)'],
    ['ap-northeast-2', 'Asia Pacific (Seoul)'], ['ap-northeast-3', 'Asia Pacific (Osaka)'], ['ap-southeast-1', 'Asia Pacific (Singapore)'],
    ['ap-southeast-2', 'Asia Pacific (Sydney)'], ['ap-southeast-3', 'Asia Pacific (Jakarta)'], ['ap-southeast-5', 'Asia Pacific (Malaysia)'],
  ];
  // The region the site sends through now (when it uses SES): choosing another one moves it.
  let sendingRegion = $derived(site.provider_kind === 'ses' ? site.provider_config?.region || '' : '');

  const body = () => ({ aws_integration_id: Number(awsId), dns_integration_id: dnsId ? Number(dnsId) : null, region: region || null });

  async function init() {
    integrations = await api('/integrations');
    const firstAws = integrations.find((i) => i.kind === 'aws');
    const firstDns = integrations.find((i) => i.kind !== 'aws');
    awsId = firstAws?.id ?? null;
    dnsId = firstDns ? String(firstDns.id) : '';
    region = firstAws?.summary.region ?? '';
    // Reopen with the accounts the last run used, so verification checks the right AWS account.
    const last = (await api(`/sites/${site.id}/setup`)).state?.request;
    if (last && integrations.some((i) => i.id === last.aws_integration_id)) {
      awsId = last.aws_integration_id;
      dnsId = last.dns_integration_id && integrations.some((i) => i.id === last.dns_integration_id) ? String(last.dns_integration_id) : '';
      region = last.region ?? region;
    } else if (last) {
      // The account this setup used was disconnected: don't silently verify with another one
      // (the same identity could exist there). Ask for an explicit choice.
      awsId = null;
      error = 'The AWS account used for this setup was disconnected. Choose an AWS account to check verification or run setup again.';
    }
    await refresh();
  }
  init().catch((err) => { error = err.message; if (!statusKnown) onStatus?.('unknown'); });

  let refreshSeq = 0;
  // Resolves true once the server's current status has been applied.
  async function refresh() {
    let ok = false;
    clearTimeout(poll);
    const n = ++refreshSeq;
    const forAws = awsId;
    try {
      const q = forAws ? `?aws_integration_id=${forAws}` : '';
      const r = await api(`/sites/${site.id}/setup${q}`);
      // A slower response for a previously selected account must not overwrite the current one.
      if (n !== refreshSeq || forAws !== awsId) return false;
      setup = r;
      statusKnown = true;
      ok = true;
    } catch (err) {
      error = err.message;
      // Only the first check matters to the parent: later poll failures leave the known status.
      if (!statusKnown) onStatus?.('unknown');
    }
    if (destroyed) return ok;
    // Fast polling while running; slower while DNS verification is pending.
    const v = setup.verification;
    const pending = setup.status === 'applied' && v && (v.dkim !== 'SUCCESS' || v.mail_from !== 'SUCCESS');
    if (setup.status === 'running') poll = setTimeout(refresh, 1500);
    else if (pending) poll = setTimeout(refresh, 20000);
    return ok;
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
    refresh(); // verification is per AWS account
  }

  async function applyNow() {
    const n = plan.records.filter((r) => r.action === 'create').length;
    const replaced = plan.records.filter((r) => r.action === 'replace').length;
    const dnsText = plan.dns === 'automatic'
      ? `add ${n} DNS record(s)${replaced ? ` and replace ${replaced} for the old region` : ''} in ${plan.zone}`
      : 'leave DNS to you';
    const move = sendingRegion && sendingRegion !== plan.region ? ` (moving it from ${sendingRegion})` : '';
    if (!confirm(`Set up ${plan.identity} in Amazon SES (${plan.region}), ${dnsText}, and switch ${site.name} to a new send-only key${move}?`)) return;
    busy = true;
    error = '';
    // The server may start provisioning before the request returns: report running until it
    // settles, and drop any status checks already in flight (they predate this run).
    applying = true;
    clearTimeout(poll);
    refreshSeq++;
    try {
      setup = await api(`/sites/${site.id}/setup/apply`, { method: 'POST', body: { ...planReq, confirm: true } });
      refresh();
    } catch (err) {
      error = err.message;
      // The run may have started anyway: keep the lock until the server says whether it did
      // (refresh also resumes polling if it's running).
      for (let delay = 2000; !destroyed && !(await refresh()); delay = Math.min(delay * 2, 30000)) {
        await new Promise((r) => setTimeout(r, delay));
      }
    } finally {
      applying = false;
      busy = false;
    }
  }

  // When a run finishes: tell the parent (the site now sends via SES) and re-plan, so the records
  // shown include the real DKIM values (needed when adding DNS by hand).
  // Only on a running → finished transition (not when the page opens on a finished run).
  let lastStatus = null;
  $effect(() => {
    const status = setup.status;
    if (statusKnown || applying) onStatus?.(applying ? 'running' : status);
    if (lastStatus === 'running' && (status === 'applied' || status === 'error')) {
      if (status === 'applied') onApplied?.();
      if (plan && planReq) preview();
    }
    lastStatus = status;
  });

  // Cleaning up a region the site moved away from. With DNS: deletes the old DKIM records setup
  // created; either way, forgets the region. AWS resources are removed with the listed commands.
  let cleanupMsg = $state('');
  async function cleanup(prev, deleteDns) {
    const what = deleteDns
      ? `Delete ${prev.dns_records.filter((r) => r.id).length} old ${prev.region} DKIM record(s) from ${site.domain}?`
      : `Stop listing ${prev.region}? Do this after you've removed its AWS resources.`;
    if (!confirm(what)) return;
    busy = true;
    error = cleanupMsg = '';
    try {
      const r = await api(`/sites/${site.id}/setup/cleanup`, {
        method: 'POST',
        body: deleteDns ? { id: prev.id, dns_integration_id: Number(dnsId) } : { id: prev.id, forget: true },
      });
      cleanupMsg = deleteDns ? `Deleted ${r.deleted_dns_records.length} DNS record(s).` : `${prev.region} cleared.`;
      await refresh();
    } catch (err) {
      error = err.message;
    } finally {
      busy = false;
    }
  }

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

  <!-- Saved progress shows even if the AWS integration has since been removed. -->
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
    {#if setup.verification_error}<Alert kind="warn">{setup.verification_error}</Alert>{/if}
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


  {#if cleanupMsg}<Alert kind="ok">{cleanupMsg}</Alert>{/if}
  {#each setup.previous_regions ?? [] as prev (prev.id)}
    {@const deletable = prev.dns_records.filter((r) => r.id)}
    <div class="card flush" style="margin:0 0 16px;box-shadow:none;padding:14px 16px">
      <h3 style="margin-bottom:4px">Clean up {prev.region}</h3>
      <p class="small muted">{site.name} no longer sends through {prev.region}, but its old setup there is still in place. Remove it so the old send-only key can't be used.</p>
      {#if prev.shared_with.length}
        <p class="small muted">{prev.identity} is also used by {prev.shared_with.join(', ')}, so its SES identity and DKIM records stay.</p>
      {/if}
      {#if prev.dns_records.length && prev.dns_removable}
        <div class="small" style="margin:10px 0 4px"><strong>DNS records</strong> (old DKIM)</div>
        <ul class="small" style="margin:0 0 8px;padding-left:18px">
          {#each prev.dns_records as r, i (r.id ?? `${r.fqdn}#${i}`)}<li><code style="word-break:break-all">{r.type} {r.fqdn} → {r.content}</code></li>{/each}
        </ul>
        {#if deletable.length && dnsId}
          <button class="secondary" onclick={() => cleanup(prev, true)} disabled={busy}>Delete these DNS records</button>
        {:else}
          <p class="small muted">Delete these at your DNS host.</p>
        {/if}
      {/if}
      <div class="small" style="margin:12px 0 4px"><strong>AWS resources</strong> in account {prev.aws_account_id}: run this with the AWS CLI, signed in to that account (your setup key isn't allowed to delete anything). It checks the account first.</div>
      <CodeBlock code={prev.commands.join('\n')} />
      <div class="actions" style="margin-top:8px"><button class="ghost" onclick={() => cleanup(prev, false)} disabled={busy}>Done — stop listing {prev.region}</button></div>
    </div>
  {/each}

  {#if !hasAws}
    <Alert kind="info">Connect an AWS account (and optionally Porkbun) on the <a href="#/integrations{returnTo ? `?return=${returnTo}` : ''}">Integrations</a> page to enable automatic setup.</Alert>
  {:else}
    <div class="grid" style="grid-template-columns:repeat(auto-fit,minmax(180px,1fr));margin-bottom:4px">
      <label class="field">AWS account
        <select bind:value={awsId} onchange={() => { error = ''; selectionChanged(); }} disabled={busy}>
          {#if awsId === null}<option value={null} disabled>Choose an AWS account…</option>{/if}
          {#each integrations.filter((i) => i.kind === 'aws') as i (i.id)}<option value={i.id}>{i.name}</option>{/each}
        </select>
      </label>
      <label class="field">DNS
        <select bind:value={dnsId} onchange={selectionChanged} disabled={busy}>
          {#each integrations.filter((i) => i.kind !== 'aws') as i (i.id)}<option value={String(i.id)}>{i.name}: add records automatically</option>{/each}
          <option value="">I'll add DNS records myself</option>
        </select>
      </label>
      <label class="field">SES region
        <select bind:value={region} onchange={selectionChanged} disabled={busy}>
          {#if region && !SES_REGIONS.some(([r]) => r === region)}<option value={region}>{region}</option>{/if}
          {#each SES_REGIONS as [r, label] (r)}<option value={r}>{r} · {label}</option>{/each}
        </select>
      </label>
    </div>
    {#if sendingRegion && region && region !== sendingRegion}
      <Alert kind="info">
        This moves {site.name} from <strong>{sendingRegion}</strong> to <strong>{region}</strong>. Setup creates everything in {region},
        replaces the region-specific MAIL FROM record, then switches sending over. {sendingRegion} keeps working until then.
        SES sandbox status and verified addresses are separate for each region.
      </Alert>
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
                  <td><span class="badge {r.action === 'exists' ? 'confirmed' : r.action === 'conflict' ? 'failed' : r.action === 'manual' || r.action === 'replace' ? 'pending' : 'sending'}">{r.action}</span></td>
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
