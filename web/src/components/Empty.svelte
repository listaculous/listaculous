<script>
  let { title, image = false, icon: Icon = null, children, actions } = $props();
</script>

<div class="empty fade-in">
  {#if image}
    <img src="/brand/empty-480.png" alt="" />
  {:else if Icon}
    <div class="empty-icon"><Icon size={26} /></div>
  {/if}
  <h3>{title}</h3>
  {#if children}<p>{@render children()}</p>{/if}
  {#if actions}<div class="actions" style="justify-content:center">{@render actions()}</div>{/if}
</div>
