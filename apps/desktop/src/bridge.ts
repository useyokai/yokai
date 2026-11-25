import { invoke, isTauri } from '@tauri-apps/api/core';
import type {
  SaveDraftRequest,
  SubmitRequest,
  ViewSnapshot,
} from './generated/desktop-contract';

export const hasNativeHost = isTauri;

export const desktop = {
  snapshot: () => invoke<ViewSnapshot>('view_snapshot'),
  connect: () => invoke<ViewSnapshot>('connect'),
  submit: (request: SubmitRequest) => invoke<ViewSnapshot>('submit', { request }),
  reconcile: () => invoke<ViewSnapshot>('reconcile'),
  saveDraft: (request: SaveDraftRequest) => invoke<ViewSnapshot>('save_draft', { request }),
};
