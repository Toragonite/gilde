/**
 * Test fixtures for the New session sheet (imported by tests only).
 */

import type { NewSessionOptions } from '../lib/gen/NewSessionOptions';

export function options(extra: Partial<NewSessionOptions> = {}): NewSessionOptions {
  return {
    vendors: [
      { vendor: 'claude', installed: true, version: '2.1.307' },
      { vendor: 'codex', installed: true, version: '0.46.0' },
      { vendor: 'agy', installed: false, version: null },
    ],
    accounts: [
      { accountId: 'a_claude_work' as never, vendor: 'claude', label: 'Work', isDefault: true, signIn: 'signedIn' },
      { accountId: 'a_claude_personal' as never, vendor: 'claude', label: 'Personal', isDefault: false, signIn: 'signedIn' },
      { accountId: 'a_codex' as never, vendor: 'codex', label: 'Default', isDefault: true, signIn: 'signedIn' },
    ],
    models: [
      { id: 'opus', label: null, isDefault: false },
      { id: 'sonnet', label: null, isDefault: true },
    ],
    modes: ['conversation', 'terminal'],
    permissionModes: ['default', 'acceptEdits', 'plan', 'dontAsk', 'auto', 'bypassPermissions'],
    locations: [{ location: { kind: 'here', locationId: 'l_main' as never }, branch: 'main', worktreeName: null }],
    teamAllowed: true,
    teamMode: 'panes',
    defaults: {
      vendor: 'claude',
      accountId: 'a_claude_work' as never,
      model: 'sonnet',
      controlMode: 'conversation',
      permissionMode: 'default',
      location: { kind: 'here', locationId: 'l_main' as never },
    },
    notices: [],
    ...extra,
  };
}
