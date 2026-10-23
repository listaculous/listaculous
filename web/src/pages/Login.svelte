<script>
  import Mail from '@lucide/svelte/icons/mail';
  import Lock from '@lucide/svelte/icons/lock';
  import Building2 from '@lucide/svelte/icons/building-2';
  import ArrowRight from '@lucide/svelte/icons/arrow-right';
  import ShieldCheck from '@lucide/svelte/icons/shield-check';
  import Zap from '@lucide/svelte/icons/zap';
  import Globe from '@lucide/svelte/icons/globe';
  import KeyRound from '@lucide/svelte/icons/key-round';
  import { onDestroy } from 'svelte';
  import { passkeysSupported, conditionalSupported, signInWithPasskey, passkeyError } from '../webauthn.js';
  import { api } from '../api.js';
  import Alert from '../components/Alert.svelte';
  import ThemeSwitch from '../components/ThemeSwitch.svelte';

  let { onLogin } = $props();
  let needsSetup = $state(false);
  let email = $state('');
  let password = $state('');
  let accountName = $state('');
  let error = $state('');
  let busy = $state(false);

  let canPasskey = passkeysSupported();
  let autofill = null; // AbortController for the background (autofill) passkey request

  api('/auth/status').then((s) => {
    needsSetup = s.needs_setup;
    if (!needsSetup) startAutofill();
  });
  onDestroy(() => autofill?.abort());

  // Lets Bitwarden & co. offer saved passkeys right in the email field's autofill menu.
  async function startAutofill() {
    if (!(await conditionalSupported())) return;
    autofill = new AbortController();
    try {
      onLogin(await signInWithPasskey({ mediation: 'conditional', signal: autofill.signal }));
    } catch (err) {
      const m = passkeyError(err);
      if (m && !autofill?.signal.aborted) error = m;
    }
  }

  async function passkeyLogin() {
    // Only one WebAuthn request can be active; cancel the autofill one first.
    autofill?.abort();
    autofill = null;
    error = '';
    busy = true;
    try {
      onLogin(await signInWithPasskey());
    } catch (err) {
      error = passkeyError(err) ?? '';
      startAutofill();
    } finally {
      busy = false;
    }
  }

  async function submit(e) {
    e.preventDefault();
    error = '';
    busy = true;
    try {
      const u = await api(needsSetup ? '/auth/setup' : '/auth/login', {
        method: 'POST',
        body: { email, password, account_name: accountName },
      });
      onLogin(u);
    } catch (err) {
      error = err.message;
    } finally {
      busy = false;
    }
  }
</script>

<div class="auth">
  <section class="auth-form">
    <div class="brand"><img src="/brand/logo-256.png" alt="" /> Listaculous</div>
    <h1>{needsSetup ? 'Welcome aboard' : 'Welcome back'}</h1>
    <p class="lead">{needsSetup ? 'Create your admin account to start sending newsletters.' : 'Sign in to manage your newsletters.'}</p>

    <form onsubmit={submit}>
      {#if needsSetup}
        <label class="field">Account name
          <div class="input-icon"><Building2 size={16} /><input bind:value={accountName} placeholder="My Company" /></div>
        </label>
      {/if}
      <label class="field">Email
        <div class="input-icon"><Mail size={16} /><input type="email" bind:value={email} required autocomplete="username webauthn" placeholder="you@example.com" /></div>
      </label>
      <label class="field">Password
        <div class="input-icon"><Lock size={16} /><input type="password" bind:value={password} required minlength={needsSetup ? 10 : undefined} autocomplete={needsSetup ? 'new-password' : 'current-password'} placeholder={needsSetup ? 'At least 10 characters' : ''} /></div>
      </label>
      {#if error}<Alert kind="error">{error}</Alert>{/if}
      <button type="submit" class="primary-grad lg block" disabled={busy}>
        {needsSetup ? 'Create account' : 'Sign in'} <ArrowRight size={16} />
      </button>
    </form>
    {#if !needsSetup && canPasskey}
      <div class="divider"><span>or</span></div>
      <button type="button" class="secondary lg block" onclick={passkeyLogin} disabled={busy}>
        <KeyRound size={17} /> Sign in with a passkey
      </button>
    {/if}
    {#if needsSetup}
      <p class="small muted" style="margin-top:16px">After setup you can add a passkey (e.g. saved in Bitwarden) on the Account page.</p>
    {/if}

    <div class="auth-foot" style="max-width:280px"><ThemeSwitch /></div>
  </section>

  <section class="auth-art" aria-hidden="true">
    <!-- Day and night artwork crossfade with the theme (see .art-* in app.css). -->
    <div class="art art-light" style="background-image:url(/brand/hero-light.jpg)"></div>
    <div class="art art-dark" style="background-image:url(/brand/hero-dark.jpg)"></div>
    <div class="quote">
      <h2>Newsletters that fly.</h2>
      <p>Self-hosted, featherweight and built for great delivery — every site, every list, one account.</p>
      <div class="pills">
        <span class="pill"><Zap size={13} /> Rust-fast & tiny</span>
        <span class="pill"><ShieldCheck size={13} /> One-click unsubscribe</span>
        <span class="pill"><Globe size={13} /> Multi-site</span>
      </div>
    </div>
  </section>
</div>
