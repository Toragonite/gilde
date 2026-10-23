/**
 * Changing an executable setting (contract §4.2.8 `settings_set_executable`,
 * §9.6): vendor binaries, the helper runtime, the agent search folder and the
 * environment names passed through.
 *
 * These values decide what Gilde runs, so the Settings list shows them
 * read-only and never edits them inline. This dialog only collects a
 * candidate; the core validates it (a regular executable owned by the user or
 * root, outside world-writable and vendor folders; names that match
 * `^[A-Z_][A-Z0-9_]{0,63}$`, ≤ 32, none reserved) and shows a native
 * confirmation naming the exact value. Nothing changes unless the user
 * confirms there (`applied: true`).
 */

import { useId, useState } from 'react';
import type { FormEvent } from 'react';

import { Sheet } from '../components/Sheet';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import type { AbsPath } from '../lib/gen/AbsPath';
import type { ExecutableKey } from '../lib/gen/ExecutableKey';
import type { ExecutableValue } from '../lib/gen/ExecutableValue';
import type { IpcError } from '../lib/ipcError';

export const ENV_NAME_RE = /^[A-Z_][A-Z0-9_]{0,63}$/;
export const MAX_ENV_NAMES = 32;

/** The value a typed path stands for, or null when it is not an absolute path. */
export function pathValue(text: string): ExecutableValue | null {
  const path = text.trim();
  if (!path.startsWith('/') || path.includes('\u0000') || path.length > 1024 || path === '/') return null;
  return { kind: 'path', path: path.replace(/\/+$/, '') as AbsPath };
}

/** The names a typed list stands for, or null when one is malformed or there are too many. */
export function envValue(text: string): ExecutableValue | null {
  const names = text
    .split(/[\s,]+/)
    .map((n) => n.trim())
    .filter((n) => n !== '');
  const unique = [...new Set(names)];
  if (unique.length > MAX_ENV_NAMES || unique.some((n) => !ENV_NAME_RE.test(n))) return null;
  return { kind: 'envNames', names: unique };
}

export interface ExecutableDialogProps {
  settingKey: ExecutableKey;
  /** What the dialog is about ("claude executable"). */
  name: string;
  /** The current value as text (a path, or names joined by spaces). */
  current: string;
  onClose: (applied: boolean) => void;
}

export function ExecutableDialog({ settingKey, name, current, onClose }: ExecutableDialogProps) {
  const { t, te } = useI18n();
  const fieldId = useId();
  const env = settingKey === 'agents.envPassthrough';
  const [text, setText] = useState(current);
  const [invalid, setInvalid] = useState(false);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const [error, setError] = useState<IpcError | null>(null);

  const apply = (event?: FormEvent) => {
    event?.preventDefault();
    if (busy) return;
    const value = env ? envValue(text) : pathValue(text);
    if (!value) {
      setInvalid(true);
      return;
    }
    setInvalid(false);
    setBusy(true);
    setNote(null);
    setError(null);
    commands.settings_set_executable({ key: settingKey, value }).then(
      (res) => {
        setBusy(false);
        if (res.applied) onClose(true);
        else setNote(t('settings.exec.cancelled'));
      },
      (e: IpcError) => {
        setBusy(false);
        setError(e);
      },
    );
  };

  return (
    <Sheet
      open
      title={t('settings.exec.title', { name })}
      onCancel={() => onClose(false)}
      onConfirm={() => apply()}
      actions={
        <>
          <button type="button" className="btn" onClick={() => onClose(false)}>
            {t('common.cancel')}
          </button>
          <button type="button" className="btn primary" disabled={busy} onClick={() => apply()}>
            {t('settings.exec.apply')}
          </button>
        </>
      }
    >
      <form className="se-exec" onSubmit={apply}>
        <p className="se-desc">{t('settings.exec.explain')}</p>
        <label htmlFor={fieldId} className="se-exec-label">
          {t(env ? 'settings.exec.envField' : 'settings.exec.pathField')}
        </label>
        <input
          id={fieldId}
          className="input mono se-exec-input"
          value={text}
          spellCheck={false}
          autoComplete="off"
          aria-invalid={invalid}
          data-autofocus=""
          onChange={(e) => {
            setText(e.target.value);
            setInvalid(false);
          }}
        />
        {invalid ? <p className="se-line" data-tone="warn">{t(env ? 'settings.exec.namesInvalid' : 'settings.exec.pathInvalid')}</p> : null}
        {note ? <p className="se-line">{note}</p> : null}
        {error ? (
          <p className="se-line" data-tone="error" role="status">
            <span>{te(error)}</span>
            {error.detail ? <span className="mono se-detail">{error.detail}</span> : null}
          </p>
        ) : null}
      </form>
    </Sheet>
  );
}
