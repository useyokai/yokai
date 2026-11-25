<script lang="ts">
  import { tick } from 'svelte';
  import type { ViewSnapshot } from '../generated/desktop-contract';
  import { pendingLabel } from '../view';

  let { snapshot }: { snapshot: ViewSnapshot | null } = $props();
  let scroller: HTMLDivElement | undefined = $state();
  let following = $state(true);
  const conversation = $derived(snapshot?.conversation);
  const pending = $derived(snapshot?.pending);

  function syncScroll() {
    if (scroller) following = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 48;
  }

  function jumpToLatest() {
    if (scroller) scroller.scrollTop = scroller.scrollHeight;
    following = true;
  }

  $effect(() => {
    const revision = snapshot?.revision;
    if (revision !== undefined && following) {
      void tick().then(() => { if (following) jumpToLatest(); });
    }
  });
</script>

<section class="conversation" aria-label="Conversation">
  <!-- svelte-ignore a11y_no_noninteractive_tabindex (Keyboard users need to scroll the transcript.) -->
  <div class="transcript" bind:this={scroller} onscroll={syncScroll}
    role="region" aria-label="Messages" tabindex="0">
    {#if conversation && !conversation.at_history_end}
      <p class="quiet history-note">This view shows part of the conversation history.</p>
    {/if}
    {#each conversation?.committed ?? [] as turn (turn.message_id)}
      <article class="turn" aria-label="Committed exchange">
        <p class="prompt">{turn.prompt}</p>
        <p class="reply">{turn.reply}</p>
      </article>
    {/each}
    {#if pending}
      <article class="turn" aria-label="Original pending exchange">
        <p class="prompt">{pending.prompt}</p>
        <p class="quiet">{pendingLabel[pending.phase]}</p>
        {#if pending.provisional_reply !== null}
          <div class="provisional">
            <p class="quiet">Provisional response</p>
            <p class="reply">{pending.provisional_reply}</p>
          </div>
        {/if}
      </article>
    {:else if !conversation?.committed.length}
      <div class="empty">
        <h2>Start a conversation</h2>
      </div>
    {/if}
  </div>
  {#if !following}
    <button class="jump" type="button" onclick={jumpToLatest}>Jump to latest</button>
  {/if}
</section>
