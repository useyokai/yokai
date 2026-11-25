<script lang="ts">
  import { onMount } from 'svelte';
  import { desktop, hasNativeHost } from './bridge';
  import { MAX_TEXT_BYTES, type ViewSnapshot } from './generated/desktop-contract';
  import Composer from './components/Composer.svelte';
  import ConnectionStatus from './components/ConnectionStatus.svelte';
  import Conversation from './components/Conversation.svelte';

  type Action = 'connect' | 'submit' | 'reconcile' | 'saveDraft';

  let snapshot = $state.raw<ViewSnapshot | null>(null);
  let draft = $state('');
  let actionBusy = $state(false);
  let nativeAvailable = $state(false);
  let degraded = $state(false);
  let feedback = $state('');
  const encoder = new TextEncoder();
  let draftEdited = false;
  let editVersion = 0;
  let mounted = false;
  let poll: Promise<void> | null = null;
  let lastPoll = -Infinity;

  let bytes = $derived(encoder.encode(draft).length);
  let canSubmit = $derived(
    nativeAvailable && !degraded && snapshot?.availability.submit === true
      && !actionBusy && bytes <= MAX_TEXT_BYTES && draft.trim().length > 0
  );
  let canSave = $derived(
    nativeAvailable && !degraded && snapshot?.availability.save_draft === true
      && !actionBusy && bytes <= MAX_TEXT_BYTES
  );

  function apply(next: ViewSnapshot) {
    snapshot = next;
    degraded = false;
    if (!draftEdited) draft = next.draft;
  }

  function edit(text: string) {
    draft = text;
    draftEdited = true;
    editVersion += 1;
    feedback = '';
  }

  async function readSnapshot() {
    try {
      const next = await desktop.snapshot();
      if (mounted) apply(next);
    } catch {
      if (mounted) degraded = true;
    } finally {
      poll = null;
    }
  }

  function pollVisible() {
    const now = performance.now();
    if (mounted && !document.hidden && nativeAvailable && !poll && !actionBusy && now - lastPoll >= 125) {
      lastPoll = now;
      poll = readSnapshot();
    }
  }

  onMount(() => {
    mounted = true;
    nativeAvailable = hasNativeHost();
    const timer = window.setInterval(pollVisible, 125);
    document.addEventListener('visibilitychange', pollVisible);
    pollVisible();
    return () => {
      mounted = false;
      window.clearInterval(timer);
      document.removeEventListener('visibilitychange', pollVisible);
    };
  });

  async function act(action: Action) {
    if (actionBusy || !nativeAvailable || degraded) return;
    actionBusy = true;
    feedback = '';
    const text = draft;
    const version = editVersion;
    try {
      await poll;
      if (!mounted || degraded) return;
      const allowed = snapshot?.availability;
      let next: ViewSnapshot;
      switch (action) {
        case 'connect':
          if (!allowed?.connect) return;
          next = await desktop.connect();
          break;
        case 'submit':
          if (!allowed?.submit || !text.trim() || encoder.encode(text).length > MAX_TEXT_BYTES) return;
          next = await desktop.submit({ text });
          break;
        case 'reconcile':
          if (!allowed?.reconcile) return;
          next = await desktop.reconcile();
          break;
        case 'saveDraft':
          if (!allowed?.save_draft || encoder.encode(text).length > MAX_TEXT_BYTES) return;
          next = await desktop.saveDraft({ text });
          break;
      }
      if (!mounted) return;
      const saved = action === 'saveDraft' && next.draft === text;
      const tracked = action === 'submit'
        && next.pending?.prompt === text && next.pending.phase !== 'definitely_rejected';
      if ((saved || tracked) && editVersion === version && !next.failure) draftEdited = false;
      apply(next);
    } catch {
      if (!mounted) return;
      degraded = true;
      feedback = action === 'submit'
        ? 'The host did not confirm the action. Check the original message before sending again.'
        : 'The host could not confirm the action.';
    } finally {
      actionBusy = false;
    }
  }
</script>

<main class="desktop" aria-label="Yokai conversation">
  <header class="titlebar"><h1>Yokai</h1></header>
  <Conversation {snapshot} />
  <div class="input-dock">
    <ConnectionStatus
      {snapshot}
      {nativeAvailable}
      {degraded}
      {actionBusy}
      onconnect={() => void act('connect')}
      onreconcile={() => void act('reconcile')}
    />
    <Composer
      {draft}
      {bytes}
      {canSubmit}
      {canSave}
      {feedback}
      onedit={edit}
      onsubmit={() => void act('submit')}
      onsave={() => void act('saveDraft')}
    />
  </div>
</main>
