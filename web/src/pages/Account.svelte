<script>
  import KeyRound from '@lucide/svelte/icons/key-round';
  import Fingerprint from '@lucide/svelte/icons/fingerprint-pattern';
  import Palette from '@lucide/svelte/icons/palette';
  import Plus from '@lucide/svelte/icons/plus';
  import Pencil from '@lucide/svelte/icons/pencil';
  import Trash2 from '@lucide/svelte/icons/trash-2';
  import Check from '@lucide/svelte/icons/check';
  import X from '@lucide/svelte/icons/x';
  import Bot from '@lucide/svelte/icons/bot';
  import BookOpen from '@lucide/svelte/icons/book-open';
  import CodeBlock from '../components/CodeBlock.svelte';
  import { api, fmtDate } from '../api.js';
  import { passkeysSupported, registerPasskey, passkeyError } from '../webauthn.js';
  import Alert from '../components/Alert.svelte';
  import Empty from '../components/Empty.svelte';
  import ThemeSwitch from '../components/ThemeSwitch.svelte';

  let current = $state('');
  let next = $state('');
  let msg = $state('');
  let error = $state('');

  let passkeys = $state([]);
  let pkName = $state('');
  let pkBusy = $state(false);
  let pkMsg = $state('');
  let pkError = $state('');
  let editing = $state(null);
  let editName = $state('');
  const supported = passkeysSupported();

  let keys = $state([]);
  let keyName = $state('');
  let keyExpiry = $state('');
  let newKey = $state(null);
  let keyError = $state('');
  let keyBusy = $state(false);

  async function loadKeys() {
    keys = await api('/auth/api-keys');
  }
  loadKeys();

  async function createKey(e) {
    e.preventDefault();
    // A second concurrent request would replace the first one-time token before it's copied.
    if (keyBusy) return;
    keyBusy = true;
    keyError = '';
    try {
      newKey = await api('/auth/api-keys', {
        method: 'POST',
        body: { name: keyName, expires_in_days: keyExpiry ? Number(keyExpiry) : null },
      });
      keyName = '';
      loadKeys();
    } catch (err) {
      keyError = err.message;
    } finally {
      keyBusy = false;
    }
  }

  async function revokeKey(k) {
    if (!confirm(`Revoke "${k.name}"? Anything using it will stop working immediately.`)) return;
    await api(`/auth/api-keys/${k.id}`, { method: 'DELETE' });
    if (newKey?.id === k.id) newKey = null;
    loadKeys();
  }

  async function loadPasskeys() {
    passkeys = await api('/auth/passkeys');
  }
  loadPasskeys();

  function guessName() {
    const ua = navigator.userAgent;
    const os = /Mac/.test(ua) ? 'Mac' : /iPhone|iPad/.test(ua) ? 'iPhone' : /Android/.test(ua) ? 'Android' : /Windows/.test(ua) ? 'Windows' : 'Device';
    return `Passkey (${os})`;
  }

  async function addPasskey(e) {
    e.preventDefault();
    pkMsg = pkError = '';
    pkBusy = true;
    try {
      await registerPasskey(pkName.trim() || guessName());
      pkMsg = 'Passkey added. You can now sign in with it.';
      pkName = '';
      loadPasskeys();
    } catch (err) {
      pkError = passkeyError(err) ?? '';
    } finally {
      pkBusy = false;
    }
  }

  async function saveName(p) {
    try {
      await api(`/auth/passkeys/${p.id}`, { method: 'PUT', body: { name: editName } });
      editing = null;
      loadPasskeys();
    } catch (err) {
      pkError = err.message;
    }
  }

  async function removePasskey(p) {
    if (!confirm(`Remove "${p.name}"? You won't be able to sign in with it anymore. (Also delete it from your password manager.)`)) return;
    await api(`/auth/passkeys/${p.id}`, { method: 'DELETE' });
    loadPasskeys();
  }

  async function submit(e) {
    e.preventDefault();
    msg = error = '';
    try {
      await api('/auth/password', { method: 'POST', body: { current_password: current, new_password: next } });
      msg = 'Password updated. Other sessions were signed out.';
      current = next = '';
    } catch (err) {
      error = err.message;
    }
  }
</script>

<div class="page-head"><div><h1>Account</h1><p class="sub">Your preferences and sign-in methods.</p></div></div>

<div style="max-width:960px">
  <div class="card">
    <div class="card-head">
      <div><h2 style="display:flex;gap:8px;align-items:center"><Fingerprint size={17} /> Passkeys</h2>
        <p class="muted small">Sign in without a password using a passkey saved in Bitwarden, iCloud Keychain, 1Password, Google or a security key.</p></div>
    </div>

    {#if pkMsg}<Alert kind="ok">{pkMsg}</Alert>{/if}
    {#if pkError}<Alert kind="error">{pkError}</Alert>{/if}

    {#if passkeys.length === 0}
      <Empty title="No passkeys yet" icon={KeyRound}>Add one below — your password manager will offer to save it.</Empty>
    {:else}
      <div class="table-wrap" style="margin:0 -20px 16px;border-top:1px solid var(--border);border-bottom:1px solid var(--border)">
        <table>
          <thead><tr><th>Name</th><th>Added</th><th>Last used</th><th></th></tr></thead>
          <tbody>
            {#each passkeys as p (p.id)}
              <tr>
                <td>
                  {#if editing === p.id}
                    <form class="actions" style="flex-wrap:nowrap" onsubmit={(e) => { e.preventDefault(); saveName(p); }}>
                      <input bind:value={editName} maxlength="60" style="max-width:260px" />
                      <button class="icon-btn" aria-label="Save name"><Check size={15} /></button>
                      <button type="button" class="ghost icon-btn" aria-label="Cancel" onclick={() => (editing = null)}><X size={15} /></button>
                    </form>
                  {:else}
                    <span class="cell-strong" style="display:inline-flex;gap:8px;align-items:center"><KeyRound size={15} /> {p.name}</span>
                  {/if}
                </td>
                <td class="small muted">{fmtDate(p.created_at)}</td>
                <td class="small muted">{p.last_used_at ? fmtDate(p.last_used_at) : 'Never'}</td>
                <td class="actions-cell">
                  <span class="row-actions">
                    <button class="link" onclick={() => { editing = p.id; editName = p.name; }} aria-label="Rename"><Pencil size={14} /></button>
                    <button class="link danger-text" onclick={() => removePasskey(p)} aria-label="Remove"><Trash2 size={14} /></button>
                  </span>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}

    {#if supported}
      <form class="actions" style="flex-wrap:nowrap;max-width:560px" onsubmit={addPasskey}>
        <input bind:value={pkName} maxlength="60" placeholder="Name, e.g. Bitwarden" />
        <button disabled={pkBusy}><Plus size={16} /> Add passkey</button>
      </form>
    {:else}
      <Alert kind="warn">This browser doesn't support passkeys.</Alert>
    {/if}
  </div>

  <div class="card">
    <div class="card-head">
      <div><h2 style="display:flex;gap:8px;align-items:center"><Bot size={17} /> API keys</h2>
        <p class="muted small">For scripts and AI agents. A key can do anything you can in the app, except manage passwords, passkeys and keys.</p></div>
      <a class="btn secondary" href="/docs" target="_blank" rel="noopener"><BookOpen size={15} /> API docs</a>
    </div>

    {#if newKey}
      <Alert kind="ok"><strong>{newKey.name}</strong> created. Copy the key now — it won't be shown again.</Alert>
      <div style="margin-bottom:12px"><CodeBlock code={newKey.token} /></div>
      <div style="margin-bottom:16px"><CodeBlock code={`curl -H "Authorization: Bearer ${newKey.token}" ${location.origin}/api/sites`} /></div>
    {/if}
    {#if keyError}<Alert kind="error">{keyError}</Alert>{/if}

    {#if keys.length}
      <div class="table-wrap" style="margin:0 -20px 16px;border-top:1px solid var(--border);border-bottom:1px solid var(--border)">
        <table>
          <thead><tr><th>Name</th><th>Key</th><th>Last used</th><th>Expires</th><th></th></tr></thead>
          <tbody>
            {#each keys as k (k.id)}
              <tr>
                <td class="cell-strong">{k.name}</td>
                <td><code>{k.prefix}…</code></td>
                <td class="small muted">{k.last_used_at ? fmtDate(k.last_used_at) : 'Never'}</td>
                <td class="small muted">{k.expires_at ? fmtDate(k.expires_at) : 'Never'}</td>
                <td class="actions-cell"><button class="link danger-text" onclick={() => revokeKey(k)}>Revoke</button></td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}

    <form class="actions" style="flex-wrap:nowrap;max-width:640px" onsubmit={createKey}>
      <input bind:value={keyName} required maxlength="60" placeholder="Name, e.g. Claude agent" />
      <select bind:value={keyExpiry} style="width:auto" aria-label="Expires">
        <option value="">Never expires</option>
        <option value="30">30 days</option>
        <option value="90">90 days</option>
        <option value="365">1 year</option>
      </select>
      <button disabled={keyBusy}><Plus size={16} /> Create key</button>
    </form>
  </div>

  <div class="grid-2">
    <div class="card">
      <div class="card-head"><div><h2 style="display:flex;gap:8px;align-items:center"><Palette size={17} /> Appearance</h2>
        <p class="muted small">System follows your operating system's light or dark setting.</p></div></div>
      <ThemeSwitch />
    </div>
    <div class="card">
      <div class="card-head"><div><h2 style="display:flex;gap:8px;align-items:center"><KeyRound size={17} /> Change password</h2>
        <p class="muted small">Changing it signs out every other session. Passkeys keep working.</p></div></div>
      <form onsubmit={submit}>
        <label class="field">Current password <input type="password" bind:value={current} required autocomplete="current-password" /></label>
        <label class="field">New password <input type="password" bind:value={next} required minlength="10" autocomplete="new-password" placeholder="At least 10 characters" /></label>
        {#if error}<Alert kind="error">{error}</Alert>{/if}
        {#if msg}<Alert kind="ok">{msg}</Alert>{/if}
        <button>Update password</button>
      </form>
    </div>
  </div>
</div>
