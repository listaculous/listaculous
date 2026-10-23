<script>
  import { untrack } from 'svelte';
  import Cloud from '@lucide/svelte/icons/cloud';
  import Server from '@lucide/svelte/icons/server';
  import Terminal from '@lucide/svelte/icons/terminal';
  import Lightbulb from '@lucide/svelte/icons/lightbulb';
  import Save from '@lucide/svelte/icons/save';
  import { api } from '../api.js';
  import Alert from '../components/Alert.svelte';
  let { site = null, onSaved } = $props();

  const blank = {
    name: '', domain: '', from_name: '', from_email: '', reply_to: '', physical_address: '',
    provider_kind: 'ses', provider_config: {}, send_rate: 10,
  };
  // The form edits a copy of the site as it was when opened.
  let f = $state(untrack(() => (site ? { ...blank, ...$state.snapshot(site) } : structuredClone(blank))));
  let error = $state('');
  let saved = $state(false);
  let busy = $state(false);

  const providers = [
    { value: 'ses', label: 'Amazon SES', desc: 'Cheapest at scale', icon: Cloud },
    { value: 'smtp', label: 'SMTP', desc: 'Postmark, Mailgun, Brevo…', icon: Server },
    { value: 'log', label: 'Log only', desc: 'Testing — nothing is sent', icon: Terminal },
  ];

  async function submit(e) {
    e.preventDefault();
    error = '';
    saved = false;
    busy = true;
    try {
      const body = { ...$state.snapshot(f), send_rate: Number(f.send_rate) };
      const s = await api(site ? `/sites/${site.id}` : '/sites', { method: site ? 'PUT' : 'POST', body });
      saved = true;
      onSaved?.(s);
    } catch (err) {
      error = err.message;
    } finally {
      busy = false;
    }
  }
</script>

<form onsubmit={submit}>
  <div class="section-title">Website</div>
  <div class="grid-2">
    <label class="field">Site name <input bind:value={f.name} required placeholder="My Blog" autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
    <label class="field">Website domain <input bind:value={f.domain} required placeholder="example.com" autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
  </div>

  <div class="section-title">Sender</div>
  <div class="grid-2">
    <label class="field">From name <input bind:value={f.from_name} required placeholder="My Blog" autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
    <label class="field">From email <input type="email" bind:value={f.from_email} required placeholder="hello@news.example.com" autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
    <label class="field">Reply-to <span class="muted">(optional)</span> <input type="email" bind:value={f.reply_to} placeholder="you@example.com" autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
    <label class="field">Send rate <span class="muted">(emails / second)</span> <input type="number" min="1" max="500" bind:value={f.send_rate} autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" />
      <span class="hint">Match your provider's limit. SES starts at 1/s in the sandbox, often 14/s+ in production.</span>
    </label>
  </div>
  <Alert kind="info">
    <strong>Tip:</strong> send from a subdomain such as <code>news.example.com</code> and verify that subdomain with your provider.
    It keeps newsletter reputation separate from your everyday mail.
  </Alert>
  <label class="field">Physical mailing address
    <textarea rows="2" bind:value={f.physical_address} placeholder="123 Main St, Springfield, UT 84000, USA" style="font-family:var(--font)"></textarea>
    <span class="hint">Required by CAN-SPAM and shown in every email footer. A PO box or registered agent address is fine.</span>
  </label>

  <div class="section-title">Email provider</div>
  <div class="choices" role="radiogroup" aria-label="Email provider">
    {#each providers as p (p.value)}
      <label class="choice">
        <input type="radio" name="provider" value={p.value} bind:group={f.provider_kind} />
        <span class="ci"><p.icon size={18} /></span>
        <div><strong>{p.label}</strong><span>{p.desc}</span></div>
      </label>
    {/each}
  </div>

  {#if f.provider_kind === 'ses'}
    <div class="grid-2">
      <label class="field">AWS region <input bind:value={f.provider_config.region} placeholder="us-east-1" required autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
      <label class="field">Configuration set <span class="muted">(optional)</span> <input bind:value={f.provider_config.configuration_set} autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
      <label class="field">Access key ID <input bind:value={f.provider_config.access_key_id} placeholder="Leave blank to use AWS_* env vars" autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
      <label class="field">Secret access key <input type="password" bind:value={f.provider_config.secret_access_key} placeholder="Leave blank to keep current / use env" autocomplete="new-password" /></label>
    </div>
    <p class="muted small" style="display:flex;gap:8px"><Lightbulb size={15} style="flex-shrink:0;margin-top:2px" />
      <span>The IAM user only needs <code>ses:SendRawEmail</code>. Verify your sending domain in SES and publish its DKIM, SPF and DMARC records.</span></p>
  {:else if f.provider_kind === 'smtp'}
    <div class="grid-2">
      <label class="field">Host <input bind:value={f.provider_config.host} required placeholder="smtp.postmarkapp.com" autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
      <label class="field">Port <input type="number" bind:value={f.provider_config.port} placeholder="587" autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
      <label class="field">Security
        <select bind:value={f.provider_config.security}>
          <option value="starttls">STARTTLS (587)</option>
          <option value="tls">TLS (465)</option>
          <option value="none">None (local relay only)</option>
        </select>
      </label>
      <label class="field">Username <input bind:value={f.provider_config.username} autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
      <label class="field">Password <input type="password" bind:value={f.provider_config.password} placeholder="Leave blank to keep current" autocomplete="new-password" /></label>
    </div>
  {:else}
    <Alert kind="warn">Emails are only written to the server log. Great for trying things out — switch to a real provider before launching.</Alert>
  {/if}

  {#if error}<Alert kind="error">{error}</Alert>{/if}
  {#if saved && site}<Alert kind="ok">Saved.</Alert>{/if}
  <div class="actions" style="margin-top:8px">
    <button class="lg" disabled={busy}><Save size={16} /> {site ? 'Save changes' : 'Create site'}</button>
  </div>
</form>
