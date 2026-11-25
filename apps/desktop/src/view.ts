import type { FailureKind, IntentPhase, ViewSnapshot, WriteCertainty } from './generated/desktop-contract';

export const pendingLabel: Record<IntentPhase, string> = {
  creating_conversation: 'Starting conversation',
  submitting_message: 'Sending',
  acceptance_unknown: 'Message acceptance unconfirmed',
  accepted: 'Message accepted',
  completion_unknown: 'Response completion unconfirmed',
  definitely_rejected: 'Message rejected',
  held: 'Work held',
};

const failureLabel: Record<FailureKind, string> = {
  authentication: 'Runtime identity could not be verified.',
  persistence: 'The host could not save local conversation state.',
  transport: 'The connection was interrupted.',
  timeout: 'The operation reached its time limit.',
  shutdown_unconfirmed: 'Runtime shutdown could not be confirmed.',
  projection_limit: 'The conversation exceeds the supported view limit.',
  rejected: 'The runtime rejected the operation.',
};

const certaintyLabel: Record<WriteCertainty, string> = {
  no_mutation: '',
  definitely_not_written: 'The operation was not sent.',
  unknown: 'The last operation may still have taken effect.',
  durably_accepted: 'The original message remains accepted.',
};

export function failureText(snapshot: ViewSnapshot | null): string {
  const failure = snapshot?.failure;
  if (!failure) return '';
  return [failureLabel[failure.kind], certaintyLabel[failure.write_certainty]]
    .filter(Boolean)
    .join(' ');
}
