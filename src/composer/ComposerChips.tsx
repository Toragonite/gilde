/**
 * The composer's chips (ux-ia §3.4 "Composer chips"): model (`⇧⌘M`),
 * permission mode and account.
 *
 *  - Model: the options come from `session_models` (the live list, F-23);
 *    a change applies from the next request (Conversation) or turn
 *    (turn-based) and says so; Terminal sessions show the model read-only
 *    ("Change it in the terminal.").
 *  - Permission mode: the chip shows `Session.permissionMode`, which follows
 *    the CLI's `system/status` (after a plan is approved it changes by
 *    itself); the menu offers `caps.setPermissionMode` only. A running claude
 *    Conversation session whose CLI reported a mode Gilde does not know has
 *    no mode (A33.3): the chip says "Unknown mode", never a previous one.
 *  - Account: the current account; another account of the same vendor means
 *    "Continue on {account}" (`session_continue` OtherAccount, ux-ia §5.8).
 */

import { useCallback, useId, useRef, useState } from 'react';

import { Popover, popoverTriggerProps } from '../components/Popover';
import { useHotkeys } from '../hooks/useHotkeys';
import type { MessageKey } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import type { Account } from '../lib/gen/Account';
import type { ModelOption } from '../lib/gen/ModelOption';
import type { PermissionMode } from '../lib/gen/PermissionMode';
import type { Session } from '../lib/gen/Session';
import { newClientRequestId } from '../lib/ids';
import type { IpcError } from '../lib/ipcError';
import { useAppStore, useShallow } from '../store/useAppStore';

export const PERMISSION_KEYS: Readonly<Record<PermissionMode, MessageKey>> = {
  default: 'session.permission.default',
  acceptEdits: 'session.permission.acceptEdits',
  plan: 'session.permission.plan',
  dontAsk: 'session.permission.dontAsk',
  auto: 'session.permission.auto',
  bypassPermissions: 'session.permission.bypassPermissions',
};

type Note = { key: string; params: Readonly<Record<string, unknown>> } | null;

/**
 * A33.3: the CLI of a running claude Conversation session reported a
 * permission mode Gilde does not know — the session has no mode
 * (`permissionMode` null) once its CLI has reported (its model is known;
 * before that World still holds the launch mode).
 */
export function modeUnknown(session: Session): boolean {
  return (
    session.vendor === 'claude' &&
    session.control.kind === 'inGilde' &&
    session.control.mode === 'conversation' &&
    session.permissionMode === null &&
    session.model !== null
  );
}

export function ComposerChips({ session, active, compact }: { session: Session; active: boolean; compact: boolean }) {
  const { td } = useI18n();
  const [note, setNote] = useState<Note>(null);
  const fail = (e: unknown) => {
    const error = e as IpcError;
    setNote({ key: error.key, params: error.params });
  };

  return (
    <div className="g-composer-chips">
      <ModelChip session={session} active={active} onNote={setNote} onError={fail} />
      {!compact && <PermissionChip session={session} onNote={setNote} onError={fail} />}
      {!compact && <AccountChip session={session} onError={fail} />}
      {note && <span className="g-composer-chipnote">{td(note.key, note.params)}</span>}
    </div>
  );
}

function ModelChip({
  session,
  active,
  onNote,
  onError,
}: {
  session: Session;
  active: boolean;
  onNote: (n: Note) => void;
  onError: (e: unknown) => void;
}) {
  const { t } = useI18n();
  const anchor = useRef<HTMLButtonElement>(null);
  const id = useId();
  const [open, setOpen] = useState(false);
  const [models, setModels] = useState<ModelOption[] | null>(null);
  const label = session.model ?? t('session.model.default');

  const show = useCallback(() => {
    if (!session.caps.setModel) {
      onNote({ key: 'model.terminal', params: {} });
      return;
    }
    setOpen(true);
    commands.session_models({ uid: session.uid }).then((res) => setModels(res.models), (e) => {
      setOpen(false);
      onError(e);
    });
  }, [onError, onNote, session.caps.setModel, session.uid]);

  useHotkeys([{ id: 'model.menu', combo: 'mod+shift+m', run: show }], { scope: 'S', enabled: active });

  const pick = (model: string | null) => {
    setOpen(false);
    commands.session_set_model({ uid: session.uid, model }).then(() => onNote({ key: 'model.nextMessage', params: {} }), onError);
  };

  return (
    <>
      <button
        ref={anchor}
        type="button"
        className="g-chipbtn"
        aria-label={`${t('session.model.label')}: ${label}`}
        aria-keyshortcuts="Meta+Shift+M"
        {...popoverTriggerProps(open, id, 'menu')}
        onClick={() => (open ? setOpen(false) : show())}
      >
        {label} <span aria-hidden="true">▾</span>
      </button>
      <Popover open={open} anchor={anchor} onClose={() => setOpen(false)} role="menu" id={id} label={t('session.model.label')}>
        {models === null ? (
          <p className="g-composer-menuline">{t('common.loading')}</p>
        ) : (
          models.map((m) => (
            <button
              key={m.id}
              type="button"
              role="menuitem"
              aria-current={m.id === session.model ? 'true' : undefined}
              onClick={() => pick(m.isDefault ? null : m.id)}
            >
              {m.label ?? m.id}
            </button>
          ))
        )}
      </Popover>
    </>
  );
}

function PermissionChip({ session, onNote, onError }: { session: Session; onNote: (n: Note) => void; onError: (e: unknown) => void }) {
  const { t } = useI18n();
  const anchor = useRef<HTMLButtonElement>(null);
  const id = useId();
  const [open, setOpen] = useState(false);
  const mode = session.permissionMode;
  const unknown = modeUnknown(session);
  if (mode === null && !unknown && session.caps.setPermissionMode.length === 0) return null;
  const label = mode ? t(PERMISSION_KEYS[mode]) : t(unknown ? 'composer.modeUnknown' : 'common.unknown');
  const options = session.caps.setPermissionMode;

  const pick = (next: PermissionMode) => {
    setOpen(false);
    commands.session_set_permission_mode({ uid: session.uid, mode: next }).then(() => onNote({ key: 'model.nextMessage', params: {} }), onError);
  };

  if (options.length === 0) {
    return (
      <span className="g-chipbtn" data-readonly="true" aria-label={`${t('session.permission.label')}: ${label}`}>
        {label}
      </span>
    );
  }
  return (
    <>
      <button
        ref={anchor}
        type="button"
        className="g-chipbtn"
        aria-label={`${t('session.permission.label')}: ${label}`}
        {...popoverTriggerProps(open, id, 'menu')}
        onClick={() => setOpen((o) => !o)}
      >
        {label} <span aria-hidden="true">▾</span>
      </button>
      <Popover open={open} anchor={anchor} onClose={() => setOpen(false)} role="menu" id={id} label={t('session.permission.label')}>
        {options.map((m) => (
          <button key={m} type="button" role="menuitem" aria-current={m === mode ? 'true' : undefined} onClick={() => pick(m)}>
            {t(PERMISSION_KEYS[m])}
          </button>
        ))}
      </Popover>
    </>
  );
}

function AccountChip({ session, onError }: { session: Session; onError: (e: unknown) => void }) {
  const { t } = useI18n();
  const anchor = useRef<HTMLButtonElement>(null);
  const id = useId();
  const [open, setOpen] = useState(false);
  const openSession = useAppStore((s) => s.openSession);
  const others = useAppStore(
    useShallow((s) => {
      const out: Account[] = [];
      for (const a of s.world.accounts.values()) {
        if (a.vendor === session.vendor && a.id !== session.accountId && a.usableForNew) out.push(a);
      }
      return out;
    }),
  );
  const label = session.details.accountLabel;
  if (label === null) return null;
  const canSwitch = session.caps.continueOptions.includes('otherAccount') && others.length > 0;

  if (!canSwitch) {
    return (
      <span className="g-chipbtn" data-readonly="true" aria-label={`${t('session.account.label')}: ${label}`}>
        {label}
      </span>
    );
  }
  const pick = (account: Account) => {
    setOpen(false);
    commands
      .session_continue({
        clientRequestId: newClientRequestId(),
        uid: session.uid,
        how: { kind: 'otherAccount', accountId: account.id },
        controlMode: null,
        model: null,
        permissionMode: null,
      })
      .then((res) => openSession(res.uid), onError);
  };
  return (
    <>
      <button
        ref={anchor}
        type="button"
        className="g-chipbtn"
        aria-label={`${t('session.account.label')}: ${label}`}
        {...popoverTriggerProps(open, id, 'menu')}
        onClick={() => setOpen((o) => !o)}
      >
        {label} <span aria-hidden="true">▾</span>
      </button>
      <Popover open={open} anchor={anchor} onClose={() => setOpen(false)} role="menu" id={id} label={t('session.account.label')}>
        {others.map((a) => (
          <button key={a.id} type="button" role="menuitem" onClick={() => pick(a)}>
            {t('action.continueOn', { account: a.label })}
          </button>
        ))}
      </Popover>
    </>
  );
}
