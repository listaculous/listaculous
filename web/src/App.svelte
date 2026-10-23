<script>
  import LayoutDashboard from '@lucide/svelte/icons/layout-dashboard';
  import Globe from '@lucide/svelte/icons/globe';
  import Mail from '@lucide/svelte/icons/mail';
  import UserRound from '@lucide/svelte/icons/user-round';
  import LogOut from '@lucide/svelte/icons/log-out';
  import Menu from '@lucide/svelte/icons/menu';
  import Plus from '@lucide/svelte/icons/plus';
  import LoaderCircle from '@lucide/svelte/icons/loader-circle';
  import { api } from './api.js';
  import { route, go } from './router.svelte.js';
  import { nav, refreshNav } from './stores.svelte.js';
  import './theme.svelte.js';
  import ThemeSwitch from './components/ThemeSwitch.svelte';
  import Login from './pages/Login.svelte';
  import Dashboard from './pages/Dashboard.svelte';
  import Site from './pages/Site.svelte';
  import NewSite from './pages/NewSite.svelte';
  import List from './pages/List.svelte';
  import Campaign from './pages/Campaign.svelte';
  import Account from './pages/Account.svelte';

  let user = $state(null);
  let ready = $state(false);
  let menuOpen = $state(false);

  async function load() {
    try {
      user = await api('/auth/me');
      refreshNav();
    } catch {
      user = null;
    }
    ready = true;
  }
  load();
  window.addEventListener('lc:unauthorized', () => (user = null));

  async function logout() {
    await api('/auth/logout', { method: 'POST' });
    user = null;
    go('/');
  }

  function onLogin(u) {
    user = u;
    refreshNav();
    go('/');
  }

  let page = $derived(route.parts[0] || '');
  let id = $derived(Number(route.parts[1]));
  let path = $derived('/' + route.parts.join('/'));

  // Close the mobile drawer on navigation.
  $effect(() => {
    path;
    menuOpen = false;
  });

  const isActive = (href) => (href === '/' ? path === '/' : path === href || path.startsWith(href + '/'));
</script>

{#if !ready}
  <div class="loading"><LoaderCircle class="spin" size={28} /></div>
{:else if !user}
  <Login {onLogin} />
{:else}
  <div class="shell">
    {#if menuOpen}<div class="scrim" onclick={() => (menuOpen = false)} role="presentation"></div>{/if}
    <aside class="sidebar" class:open={menuOpen}>
      <a class="brand" href="#/"><img src="/brand/logo-256.png" alt="" /> Listaculous</a>

      <a class="nav-link" class:active={isActive('/')} href="#/"><LayoutDashboard size={17} /> Dashboard</a>
      <a class="nav-link" class:active={isActive('/account')} href="#/account"><UserRound size={17} /> Account</a>

      <div class="nav-label">Sites</div>
      {#each nav.sites as s (s.id)}
        <a class="nav-link" class:active={isActive(`/sites/${s.id}`)} href="#/sites/{s.id}" title={s.domain}>
          <Globe size={17} /> <span class="label">{s.name}</span>
        </a>
        {#each nav.lists.filter((l) => l.site_id === s.id) as l (l.id)}
          <a class="nav-link" style="padding-left:34px" class:active={isActive(`/lists/${l.id}`)} href="#/lists/{l.id}" title={l.name}>
            <Mail size={15} /> <span class="label">{l.name}</span><span class="count">{l.confirmed}</span>
          </a>
        {/each}
      {/each}
      <a class="nav-link" class:active={isActive('/new-site')} href="#/new-site"><Plus size={17} /> Add site</a>

      <div class="sidebar-foot">
        <ThemeSwitch />
        <div class="user-row">
          <div class="avatar">{user.email[0].toUpperCase()}</div>
          <span class="email" title={user.email}>{user.email}</span>
          <button class="ghost icon-btn" onclick={logout} title="Log out" aria-label="Log out"><LogOut size={16} /></button>
        </div>
      </div>
    </aside>

    <div>
      <div class="mobile-top">
        <button class="ghost icon-btn" onclick={() => (menuOpen = true)} aria-label="Open menu"><Menu size={20} /></button>
        <a class="brand" style="padding:0;font-size:16px" href="#/"><img src="/brand/logo-256.png" alt="" style="width:28px;height:28px" /> Listaculous</a>
      </div>
      <main class="content">
        {#key path}
          <div class="fade-in">
            {#if page === 'sites' && id}
              <Site {id} />
            {:else if page === 'new-site'}
              <NewSite />
            {:else if page === 'lists' && id}
              <List {id} tab={route.parts[2] || 'subscribers'} />
            {:else if page === 'campaigns' && id}
              <Campaign {id} />
            {:else if page === 'account'}
              <Account />
            {:else}
              <Dashboard />
            {/if}
          </div>
        {/key}
      </main>
    </div>
  </div>
{/if}
