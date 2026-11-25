<script lang="ts">
  import type { ConnectionState, ViewSnapshot } from '../generated/desktop-contract';
  import { failureText, pendingLabel } from '../view';

  let { snapshot, nativeAvailable, degraded, actionBusy, onconnect, onreconcile }: {
    snapshot: ViewSnapshot | null;
    nativeAvailable: boolean;
    degraded: boolean;
    actionBusy: boolean;
    onconnect: () => void;
    onreconcile: () => void;
  } = $props();

  const labels: Record<ConnectionState, string> = {
    disconnected: 'Disconnected · Runtime work may continue',
    connecting: 'Connecting',
    connected: 'Connected',
    runtime_stopped: 'Runtime stopped',
    blocked: 'Connection blocked',
  };
  const phase = $derived(snapshot?.pending?.phase);
  const status = $derived(
    !nativeAvailable ? 'Native host unavailable'
      : degraded ? 'Host status unavailable · Showing last known data'
      : snapshot ? labels[snapshot.connection] : 'Waiting for host status'
  );
  const failure = $derived(failureText(snapshot));
</script>

<section class="connection" aria-label="Connection">
  <div role="status" aria-live="polite" aria-atomic="true">
    <span class="connection-label">{status}</span>
    {#if phase}<span class="quiet"> · {pendingLabel[phase]}</span>{/if}
    {#if failure}<p class="quiet">{failure}</p>{/if}
  </div>
  <div class="actions">
    {#if snapshot?.availability.connect && !degraded}
      <button type="button" disabled={actionBusy} onclick={onconnect}>
        {snapshot.epoch ? 'Reconnect' : 'Connect'}
      </button>
    {/if}
    {#if snapshot?.availability.reconcile && !degraded}
      <button type="button" disabled={actionBusy} onclick={onreconcile}>Check original message</button>
    {/if}
  </div>
</section>
