<script>
  // One column of an automation flowchart: the steps in order with "+" buttons between them.
  // If/else steps render their Yes and No columns with this same component.
  import Plus from '@lucide/svelte/icons/plus';
  import Mail from '@lucide/svelte/icons/mail';
  import Clock from '@lucide/svelte/icons/clock';
  import Split from '@lucide/svelte/icons/split';
  import Tag from '@lucide/svelte/icons/tag';
  import ListPlus from '@lucide/svelte/icons/list-plus';
  import Target from '@lucide/svelte/icons/target';
  import CircleStop from '@lucide/svelte/icons/circle-stop';
  import Flow from './Flow.svelte';
  let { steps, path, ctx } = $props();

  const icons = { email: Mail, wait: Clock, branch: Split, tag: Tag, list: ListPlus, goal: Target, exit: CircleStop };
  const kinds = [
    ['email', 'Send email'], ['wait', 'Wait'], ['branch', 'If/else'], ['tag', 'Tag'],
    ['list', 'Newsletter'], ['goal', 'Goal'], ['exit', 'Exit'],
  ];
</script>

{#snippet adder(i)}
  {@const key = `${path}:${i}`}
  <div class="flow-gap">
    <button class="flow-add" class:open={ctx.adding === key} disabled={!ctx.editable} title="Add a step here" aria-label="Add a step here"
      onclick={() => (ctx.adding = ctx.adding === key ? null : key)}><Plus size={14} /></button>
    {#if ctx.adding === key}
      <div class="flow-menu fade-in">
        {#each kinds as [k, label] (k)}
          {@const Icon = icons[k]}
          <button class="secondary" onclick={() => ctx.add(steps, i, k)}><Icon size={14} /> {label}</button>
        {/each}
      </div>
    {/if}
  </div>
{/snippet}

{#each steps as s, i (s.id)}
  {@const Icon = icons[s.type]}
  {@render adder(i)}
  <button class="flow-node {s.type}" class:selected={ctx.selected === s.id} class:warn={ctx.problem(s)} onclick={() => ctx.select(s.id)}>
    <span class="flow-icon"><Icon size={16} /></span>
    <span class="flow-text">
      <span class="flow-title">{ctx.title(s)}</span>
      {#if ctx.detail(s)}<span class="flow-detail">{ctx.detail(s)}</span>{/if}
    </span>
    {#if ctx.here(s)}<span class="flow-count" title="People at this step now">{ctx.here(s).toLocaleString()}</span>{/if}
  </button>
  {#if s.type === 'branch'}
    <div class="flow-branch">
      {#each [['yes', 'Yes'], ['no', 'No']] as [slot, label] (slot)}
        <div class="flow-col">
          <span class="flow-pill {slot}">{label}</span>
          <Flow steps={s[slot]} path="{s.id}.{slot}" {ctx} />
        </div>
      {/each}
    </div>
  {/if}
  {#if s.type === 'exit'}<div class="flow-end">End</div>{/if}
{/each}
{#if !steps.length || steps[steps.length - 1].type !== 'exit'}{@render adder(steps.length)}{/if}
