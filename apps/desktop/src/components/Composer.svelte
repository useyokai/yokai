<script lang="ts">
  import { MAX_TEXT_BYTES } from '../generated/desktop-contract';

  let { draft, bytes, canSubmit, canSave, feedback, onedit, onsubmit, onsave }: {
    draft: string;
    bytes: number;
    canSubmit: boolean;
    canSave: boolean;
    feedback: string;
    onedit: (text: string) => void;
    onsubmit: () => void;
    onsave: () => void;
  } = $props();

  let attempted = $state(false);
  const overLimit = $derived(bytes > MAX_TEXT_BYTES);
  const validation = $derived(
    overLimit ? 'Draft exceeds 16 KiB UTF-8.'
      : feedback || (attempted && !canSubmit ? 'Sending is unavailable. Your draft is still here.' : '')
  );

  function send() {
    if (!canSubmit) {
      attempted = true;
      return;
    }
    attempted = false;
    onsubmit();
  }

  function keydown(event: KeyboardEvent) {
    if (event.key !== 'Enter' || event.shiftKey || event.isComposing) return;
    event.preventDefault();
    if (!event.repeat) send();
  }
</script>

<form class="composer" onsubmit={(event) => { event.preventDefault(); send(); }}>
  <label for="draft">Draft message</label>
  <textarea id="draft" rows="3" value={draft} onkeydown={keydown}
    aria-invalid={overLimit}
    aria-describedby="draft-feedback"
    oninput={(event) => { attempted = false; onedit(event.currentTarget.value); }}></textarea>
  <div class="composer-footer">
    <span id="draft-feedback" class="quiet" role="status">{validation}</span>
    <div class="actions">
      <button type="button" disabled={!canSave} onclick={onsave}>Save draft</button>
      <button class="primary" type="submit" disabled={!canSubmit}>Send</button>
    </div>
  </div>
</form>
