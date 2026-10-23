<script>
  import ChevronRight from '@lucide/svelte/icons/chevron-right';
  import { go } from '../router.svelte.js';
  import { refreshNav } from '../stores.svelte.js';
  import SiteForm from './SiteForm.svelte';
</script>

<div class="crumbs"><a href="#/">Dashboard</a><ChevronRight size={14} /><span>New site</span></div>
<div class="page-head">
  <div>
    <h1>Add a site</h1>
    <p class="sub">A site is one of your websites. It has its own sender identity, email provider and newsletters.</p>
  </div>
</div>
<div class="card" style="max-width:860px">
  <SiteForm onSaved={(s) => { refreshNav(); go(`/sites/${s.id}`); }} />
</div>
