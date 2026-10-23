<script>
  import Cloud from '@lucide/svelte/icons/cloud';
  import Globe from '@lucide/svelte/icons/globe';
  import Plug from '@lucide/svelte/icons/plug';
  import Plus from '@lucide/svelte/icons/plus';
  import Trash2 from '@lucide/svelte/icons/trash-2';
  import ShieldCheck from '@lucide/svelte/icons/shield-check';
  import ExternalLink from '@lucide/svelte/icons/external-link';
  import Terminal from '@lucide/svelte/icons/terminal';
  import Download from '@lucide/svelte/icons/download';
  import LoaderCircle from '@lucide/svelte/icons/loader-circle';
  import { onDestroy } from 'svelte';
  import { api, fmtDate } from '../api.js';
  import Alert from '../components/Alert.svelte';
  import CodeBlock from '../components/CodeBlock.svelte';

  const NOFILL = { autocomplete: 'off', 'data-bwignore': '', 'data-1p-ignore': '', 'data-lpignore': 'true' };

  let items = $state([]);
  let policy = $state('');
  let awsMode = $state('cli'); // 'cli' | 'console'
  let cliRegion = $state('us-east-1');
  let cliKey = $state(null); // temporary API key used by the one-liner
  let script = $state('');
  let showScript = $state(false);
  let watch;
  onDestroy(() => clearTimeout(watch));

  // The script authenticates with a single-use "aws_setup" API key: it can only fetch the script
  // and connect one AWS account, expires in a day, and the server deletes it once it has
  // connected. So the page just waits for *its* key to disappear: no client-side revocation.
  let attempt = 0;
  let preparing = $state(false);
  let watchNote = $state('');

  const REGION_RE = /^[a-z]{2}(-[a-z]+)+-\d$/;
  let regionOk = $derived(REGION_RE.test(cliRegion));

  async function revokeSetupKey(key) {
    try {
      await api(`/auth/api-keys/${key.id}`, { method: 'DELETE' });
    } catch (err) {
      error = `Couldn't revoke setup key ${key.prefix}… (${err.message}). Revoke it under Account → API keys, or let it expire within a day; it can only connect an AWS account.`;
    }
  }

  async function prepareCli() {
    if (preparing || cliKey || !regionOk) return;
    preparing = true;
    error = '';
    const mine = ++attempt;
    try {
      const key = await api('/auth/api-keys', { method: 'POST', body: { name: 'AWS CLI setup script', scope: 'aws_setup' } });
      if (mine !== attempt) {
        // Cancelled while creating: don't leave it around (and say so if that fails).
        await revokeSetupKey(key);
        return;
      }
      cliKey = key;
      watchForConnection(mine);
    } catch (err) {
      error = err.message;
    } finally {
      preparing = false;
    }
  }

  function watchForConnection(mine) {
    clearTimeout(watch);
    watch = setTimeout(async () => {
      if (mine !== attempt) return;
      try {
        const keys = await api('/auth/api-keys');
        if (mine !== attempt) return;
        watchNote = '';
        if (!keys.some((k) => k.id === cliKey.id)) {
          // The key is gone. It was either consumed by a successful connect (the integration
          // records which setup key created it) or revoked elsewhere.
          const keyId = cliKey.id;
          await load();
          if (mine !== attempt) return;
          endCli();
          if (items.some((i) => i.details?.setup_key_id === keyId)) {
            msg = 'AWS connected from the CLI.';
            if (open === 'aws') open = null;
          } else {
            error = 'The setup key was revoked before an AWS account was connected. Get a new setup command to try again.';
          }
          return;
        }
      } catch (err) {
        if (mine !== attempt) return;
        watchNote = `Couldn't check status (${err.message}); retrying…`;
      }
      watchForConnection(mine);
    }, 3000);
  }

  // Ends the current CLI attempt (stops polling, forgets its command) without revoking.
  function endCli() {
    attempt++;
    clearTimeout(watch);
    cliKey = null;
    watchNote = '';
  }

  // Every way out of the AWS form (Cancel on either tab, connecting manually) comes through here:
  // it ends the attempt and revokes an unused setup key, reporting failures.
  async function cancelCli({ close = true } = {}) {
    const key = cliKey;
    endCli();
    if (close) open = null;
    if (key) await revokeSetupKey(key);
  }
  onDestroy(() => { attempt++; });

  async function loadScript() {
    script = await (await fetch(`/api/integrations/aws-setup-script?region=${encodeURIComponent(cliRegion)}`)).text();
    showScript = true;
  }

  function downloadScript() {
    const a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob([script], { type: 'text/x-shellscript' }));
    a.download = 'listaculous-aws-setup.sh';
    a.click();
    URL.revokeObjectURL(a.href);
  }

  let oneLiner = $derived(cliKey
    ? `curl -fsSL -H "Authorization: Bearer ${cliKey.token}" "${location.origin}/api/integrations/aws-setup-script?region=${encodeURIComponent(cliRegion)}" \\\n  | LISTACULOUS_API_KEY=${cliKey.token} bash`
    : '');
  let open = $state(null); // 'aws' | 'porkbun'
  let aws = $state({ access_key_id: '', secret_access_key: '', region: 'us-east-1' });
  let pork = $state({ api_key: '', secret_api_key: '' });
  let busy = $state(false);
  let msg = $state('');
  let error = $state('');

  async function load() {
    items = await api('/integrations');
  }
  load();
  api('/integrations/aws-policy').then((p) => (policy = JSON.stringify(p, null, 2)));

  async function connect(kind) {
    busy = true;
    msg = error = '';
    try {
      const config = kind === 'aws' ? $state.snapshot(aws) : $state.snapshot(pork);
      const r = await api('/integrations', { method: 'POST', body: { kind, config } });
      // Connected by hand: any pending CLI attempt is no longer needed.
      if (kind === 'aws') await cancelCli({ close: false });
      msg = `Connected ${r.name}.`;
      open = null;
      aws = { access_key_id: '', secret_access_key: '', region: 'us-east-1' };
      pork = { api_key: '', secret_api_key: '' };
      load();
    } catch (err) {
      error = err.message;
    } finally {
      busy = false;
    }
  }

  async function remove(i) {
    if (!confirm(`Disconnect ${i.name}? Sites already set up keep sending; you just can't run setup with it.`)) return;
    await api(`/integrations/${i.id}`, { method: 'DELETE' });
    load();
  }

  let awsItems = $derived(items.filter((i) => i.kind === 'aws'));
  let dnsItems = $derived(items.filter((i) => i.kind !== 'aws'));
</script>

<div class="page-head">
  <div>
    <h1>Integrations</h1>
    <p class="sub">Connect Amazon Web Services and your DNS host, and Listaculous can set up sending for a site automatically: SES, DKIM, SPF, DMARC and bounce handling.</p>
  </div>
</div>

{#if msg}<Alert kind="ok">{msg}</Alert>{/if}
{#if error}<Alert kind="error">{error}</Alert>{/if}

<div class="grid-2">
  <div class="card">
    <div class="card-head">
      <div style="display:flex;gap:12px;align-items:center">
        <div class="stat-icon" style="margin:0"><Cloud size={19} /></div>
        <div><h2>Amazon Web Services</h2><p class="muted small">Used to set up SES. Each site then gets its own send-only key.</p></div>
      </div>
    </div>
    {#each awsItems as i (i.id)}
      <div class="li-row">
        <div><div class="cell-strong">{i.name}</div><div class="small muted">{i.summary.access_key_id} · {i.summary.region} · added {fmtDate(i.created_at)}</div></div>
        <button class="link danger-text" onclick={() => remove(i)} aria-label="Disconnect"><Trash2 size={15} /></button>
      </div>
    {/each}
    {#if open === 'aws'}
      <div class="seg" style="margin:12px 0 4px">
        <button class:active={awsMode === 'cli'} onclick={() => (awsMode = 'cli')}><Terminal size={13} /> AWS CLI</button>
        <button class:active={awsMode === 'console'} onclick={() => (awsMode = 'console')}>AWS console</button>
      </div>
    {/if}
    {#if open === 'aws' && awsMode === 'cli'}
      <p class="small muted" style="margin:10px 0">Runs on your machine with your AWS CLI login (<code>aws sts get-caller-identity</code> should work). It creates the <code>listaculous-setup</code> IAM user with the exact policy below, makes an access key and connects it here. No console needed.</p>
      <label class="field" style="max-width:220px">Default SES region <input bind:value={cliRegion} {...NOFILL} oninput={() => (showScript = false)} aria-invalid={!regionOk} /></label>
      {#if !regionOk}<p class="small" style="color:var(--danger);margin:-8px 0 12px">Enter an AWS region like <code>us-east-1</code>.</p>{/if}
      {#if cliKey && regionOk}
        <div style="margin-bottom:10px"><CodeBlock code={oneLiner} /></div>
        <p class="small muted" style="display:flex;gap:8px;align-items:center"><LoaderCircle size={14} class="spin" /> Waiting for the script to connect… Its single-use key can only connect an AWS account, expires in a day, and is deleted as soon as AWS is connected.</p>
        {#if watchNote}<p class="small muted">{watchNote}</p>{/if}
      {:else}
        <button onclick={prepareCli} disabled={preparing || !regionOk}><Terminal size={16} /> Get the setup command</button>
      {/if}
      <div class="actions" style="margin-top:12px">
        <button class="link" disabled={!regionOk} onclick={() => (showScript ? (showScript = false) : loadScript())}>{showScript ? 'Hide the script' : 'View the script first'}</button>
        {#if showScript}<button class="link" onclick={downloadScript}><Download size={13} /> Download</button>{/if}
      </div>
      {#if showScript}
        <p class="small muted" style="margin:8px 0">Prefer to run it yourself? Download and run <code>bash listaculous-aws-setup.sh</code>; it prints the key to paste in below. Set <code>LISTACULOUS_API_KEY</code> to connect automatically, and <code>AWS_PROFILE</code> to choose a profile.</p>
        <div style="max-height:360px;overflow:auto;margin-bottom:12px"><CodeBlock code={script} /></div>
      {/if}
      <details style="margin-top:8px">
        <summary class="small">Paste a key manually</summary>
        <form style="margin-top:10px" onsubmit={(e) => { e.preventDefault(); connect('aws'); }}>
          <div class="grid-2">
            <label class="field">Access key ID <input bind:value={aws.access_key_id} required {...NOFILL} placeholder="AKIA…" /></label>
            <label class="field">Secret access key <input type="password" bind:value={aws.secret_access_key} required autocomplete="new-password" data-bwignore /></label>
            <label class="field">Default SES region <input bind:value={aws.region} required {...NOFILL} placeholder="us-east-1" /></label>
          </div>
          <button disabled={busy}><Plug size={16} /> Connect AWS</button>
        </form>
      </details>
      <div class="actions" style="margin-top:12px"><button type="button" class="ghost" onclick={cancelCli}>Cancel</button></div>
    {:else if open === 'aws'}
      <ol class="steps small" style="margin:12px 0 16px">
        <li><div>In the AWS console, open <strong>IAM → Users → Create user</strong> (e.g. <code>listaculous-setup</code>). No console access needed.</div></li>
        <li><div style="min-width:0">Attach this as an <strong>inline policy</strong> (Permissions → Add permissions → Create inline policy → JSON):
          <div style="margin-top:8px"><CodeBlock code={policy} /></div></div></li>
        <li><div>Create an <strong>access key</strong> for the user (use case: "Application running outside AWS") and paste it below.</div></li>
      </ol>
      <form onsubmit={(e) => { e.preventDefault(); connect('aws'); }}>
        <div class="grid-2">
          <label class="field">Access key ID <input bind:value={aws.access_key_id} required {...NOFILL} placeholder="AKIA…" /></label>
          <label class="field">Secret access key <input type="password" bind:value={aws.secret_access_key} required autocomplete="new-password" data-bwignore /></label>
          <label class="field">Default SES region <input bind:value={aws.region} required {...NOFILL} placeholder="us-east-1" /></label>
        </div>
        <p class="muted small" style="display:flex;gap:8px"><ShieldCheck size={15} style="flex-shrink:0;margin-top:2px" />
          <span>The key is checked with AWS and stored encrypted. It's only used to set things up; you can delete it in AWS once your sites are configured. Sending uses separate send-only keys.</span></p>
        <div class="actions"><button disabled={busy}><Plug size={16} /> Connect AWS</button><button type="button" class="ghost" onclick={() => cancelCli()}>Cancel</button></div>
      </form>
    {:else}
      <button class="secondary" style="margin-top:8px" onclick={() => (open = 'aws')}><Plus size={16} /> Connect an AWS account</button>
    {/if}
  </div>

  <div class="card">
    <div class="card-head">
      <div style="display:flex;gap:12px;align-items:center">
        <div class="stat-icon" style="margin:0"><Globe size={19} /></div>
        <div><h2>DNS: Porkbun</h2><p class="muted small">Optional. Without it, setup lists the DNS records for you to add yourself.</p></div>
      </div>
    </div>
    {#each dnsItems as i (i.id)}
      <div class="li-row">
        <div><div class="cell-strong">{i.name}</div><div class="small muted">added {fmtDate(i.created_at)}</div></div>
        <button class="link danger-text" onclick={() => remove(i)} aria-label="Disconnect"><Trash2 size={15} /></button>
      </div>
    {/each}
    {#if open === 'porkbun'}
      <ol class="steps small" style="margin:12px 0 16px">
        <li><div>Create an API key at <a href="https://porkbun.com/account/api" target="_blank" rel="noopener">porkbun.com/account/api <ExternalLink size={12} /></a>.</div></li>
        <li><div>For each domain Listaculous should manage: <strong>Domain Management → your domain → Details → API Access → on</strong>. Other domains stay untouched.</div></li>
      </ol>
      <form onsubmit={(e) => { e.preventDefault(); connect('porkbun'); }}>
        <div class="grid-2">
          <label class="field">API key <input type="password" bind:value={pork.api_key} required autocomplete="new-password" data-bwignore placeholder="pk1_…" /></label>
          <label class="field">Secret API key <input type="password" bind:value={pork.secret_api_key} required autocomplete="new-password" data-bwignore placeholder="sk1_…" /></label>
        </div>
        <p class="muted small">Listaculous only ever <strong>adds</strong> the records it needs, and never edits or deletes your existing DNS records.</p>
        <div class="actions"><button disabled={busy}><Plug size={16} /> Connect Porkbun</button><button type="button" class="ghost" onclick={() => (open = null)}>Cancel</button></div>
      </form>
    {:else}
      <button class="secondary" style="margin-top:8px" onclick={() => (open = 'porkbun')}><Plus size={16} /> Connect Porkbun</button>
    {/if}
  </div>
</div>

<div class="card">
  <h2>Next</h2>
  <p class="muted small" style="margin:4px 0 0">Open a site and go to <strong>Deliverability → Automatic setup</strong>. You'll see every change before anything happens.</p>
</div>
