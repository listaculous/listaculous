<script>
  // Edits an automation condition in place (`cond` is part of the flowchart), then calls onchange.
  let { cond, tags, lists, campaignOptions, onchange } = $props();

  function setType(type) {
    for (const k of Object.keys(cond)) delete cond[k];
    cond.type = type;
    if (type === 'opened' || type === 'clicked') cond.campaign_id = campaignOptions[0]?.id ?? 0;
    if (type === 'clicked') cond.url = '';
    if (type === 'has_tag') cond.tag_id = tags[0]?.id ?? 0;
    if (type === 'on_list') cond.list_id = lists[0]?.id ?? 0;
    onchange();
  }
</script>

<div class="cond">
  <label class="field">Condition
    <select value={cond.type} onchange={(e) => setType(e.currentTarget.value)}>
      <option value="opened">Opened an email</option>
      <option value="clicked">Clicked a link in an email</option>
      <option value="has_tag">Has a tag</option>
      <option value="on_list">Is subscribed to a newsletter</option>
    </select></label>
  {#if cond.type === 'opened' || cond.type === 'clicked'}
    <label class="field">Email
      <select value={cond.campaign_id} onchange={(e) => { cond.campaign_id = +e.currentTarget.value; onchange(); }}>
        <option value={0}>Choose an email…</option>
        {#each campaignOptions as c (c.id)}<option value={c.id}>{c.label}</option>{/each}
      </select></label>
    {#if cond.type === 'clicked'}
      <label class="field">Only this link <span class="muted">(optional)</span>
        <input type="url" value={cond.url} placeholder="https://… (blank = any link)" onchange={(e) => { cond.url = e.currentTarget.value.trim(); onchange(); }} autocomplete="off" data-bwignore data-1p-ignore data-lpignore="true" /></label>
    {:else}
      <p class="hint" style="margin-top:-8px">Opens are approximate: some mail apps load images automatically, others block them. Clicks count as opens.</p>
    {/if}
  {:else if cond.type === 'has_tag'}
    <label class="field">Tag
      <select value={cond.tag_id} onchange={(e) => { cond.tag_id = +e.currentTarget.value; onchange(); }}>
        <option value={0}>Choose a tag…</option>
        {#each tags as t (t.id)}<option value={t.id}>{t.name}</option>{/each}
      </select></label>
  {:else if cond.type === 'on_list'}
    <label class="field">Newsletter
      <select value={cond.list_id} onchange={(e) => { cond.list_id = +e.currentTarget.value; onchange(); }}>
        <option value={0}>Choose a newsletter…</option>
        {#each lists as l (l.id)}<option value={l.id}>{l.name}</option>{/each}
      </select></label>
  {/if}
</div>
