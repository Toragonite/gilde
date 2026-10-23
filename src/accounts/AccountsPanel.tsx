/**
 * The Accounts column of Accounts & health (ux-ia §3.15, §5.8; contract
 * §4.2.7, A10.4, A24.1, A24.8).
 *
 * - One radio group per vendor sets the default account for new sessions
 *   (`account_set_default`); `Space` or a click picks.
 * - A Discovered sibling (`~/.claude-*` found on this Mac) is never usable
 *   until the user confirms it here (`account_confirm`, C7).
 * - Quota bars and the sign-in state come from the store's `Account`; a share
 *   the vendor does not give reads "unknown", a vendor without numbers says so
 *   in a sentence, never an empty bar.
 * - "Spent today by session": tokens from usage reports; "unknown" is a value,
 *   never 0 (A10.4); money only when the vendor reports a cost.
 */

import { useId, useRef, useState } from 'react';
import type { FormEvent } from 'react';

import { Bar } from '../components/Bar';
import { Mono } from '../components/Mono';
import { Popover, popoverTriggerProps } from '../components/Popover';
import { MoreIcon, PlusIcon } from '../components/icons';
import type { MessageKey } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import { formatCostMicros, formatTokens } from '../lib/format';
import type { Account } from '../lib/gen/Account';
import type { AccountAddVendor } from '../lib/gen/AccountAddVendor';
import type { Quota } from '../lib/gen/Quota';
import { clock } from '../lib/clock';
import { newClientRequestId } from '../lib/ids';
import type { IpcError } from '../lib/ipcError';
import { vendorBadge } from '../lib/vendor';
import { clockTime, displayName, ownerWord } from '../shell/words';
import { useAppStore } from '../store/useAppStore';
import { accountsByVendor, displayDir, localDay, NEAR_LIMIT_PCT, orchestratorAccounts, orderedWindows, spendRows, validPct } from './model';

const WINDOW_LABEL: Readonly<Record<'fiveHour' | 'week' | 'other', MessageKey>> = {
  fiveHour: 'accounts.window.fiveHour',
  week: 'accounts.window.week',
  other: 'accounts.window.other',
};

const ADDABLE: readonly AccountAddVendor[] = ['claude', 'codex'];

/** The name an account shows: the core's "Default" label reads in the UI language. */
export function accountName(t: (k: MessageKey) => string, a: Pick<Account, 'label' | 'configDir' | 'vendor'>): string {
  if (a.label === 'Default' || a.label === '') return a.configDir === null && (a.vendor === 'cursor' || a.vendor === 'agy') ? t('accounts.defaultLogin') : t('account.default');
  return a.label;
}

const WEEK_MS = 7 * 24 * 3600_000;
const DAY_MS = 24 * 3600_000;

function resetText(at: number, now: number, locale: 'ko' | 'en'): string {
  if (at - now < DAY_MS) return clockTime(at, locale);
  const day = new Intl.DateTimeFormat(locale, { weekday: 'short' }).format(new Date(at));
  return at - now < WEEK_MS ? `${day} ${clockTime(at, locale)}` : new Intl.DateTimeFormat(locale, { month: 'short', day: 'numeric' }).format(new Date(at));
}

export function QuotaLines({ quota, signedOut }: { quota: Quota | null; signedOut: boolean }) {
  const { t, locale } = useI18n();
  if (signedOut) return null;
  if (!quota) return null;
  if (quota.unavailable === 'notReported' || (quota.unavailable === null && quota.windows.length === 0)) {
    return <p className="ah-sub">{t('accounts.noLimits')}</p>;
  }
  if (quota.unavailable === 'signedOut') return <p className="ah-sub">{t('accounts.unavailable.signedOut')}</p>;
  if (quota.unavailable === 'sidecarDown') return <p className="ah-sub">{t('accounts.unavailable.cantRead')}</p>;
  if (quota.unavailable === 'probeFailed' && quota.windows.length === 0) return <p className="ah-sub">{t('accounts.unavailable.probeFailed')}</p>;
  const now = clock.wall();
  return (
    <div className="ah-quota">
      {orderedWindows(quota.windows).map((w, i) => {
        const pct = validPct(w.usedPct);
        const label = w.label ?? t(WINDOW_LABEL[w.kind]);
        return (
          <div key={`${w.kind}-${i}`} className="ah-window">
            <span className="ah-window-label">{label}</span>
            {pct === null ? (
              <span className="ah-window-unknown">{t('common.unknown')}</span>
            ) : (
              <>
                <Bar value={pct / 100} warnAt={0.7} criticalAt={NEAR_LIMIT_PCT / 100} label={t('accounts.windowMeter', { window: label, pct })} />
                <Mono className="ah-window-pct">{`${pct}%`}</Mono>
              </>
            )}
            <span className="ah-window-reset">{w.resetsAt !== null ? t('accounts.resets', { time: resetText(w.resetsAt, now, locale) }) : ''}</span>
          </div>
        );
      })}
      {quota.stale ? <p className="ah-sub">{t('accounts.stale')}</p> : null}
    </div>
  );
}

function AccountRow({ account, onError }: { account: Account; onError: (e: IpcError | null) => void }) {
  const { t } = useI18n();
  const openSession = useAppStore((s) => s.openSession);
  const [menuOpen, setMenuOpen] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const [label, setLabel] = useState(account.label);
  const menuButton = useRef<HTMLButtonElement>(null);
  const menuId = useId();
  const fieldId = useId();
  const name = accountName(t, account);
  const dir = displayDir(account.configDir);
  const discovered = account.source === 'discovered' && !account.usableForNew;
  const signedOut = account.signIn === 'signedOut';
  const run = (p: Promise<unknown>) => {
    onError(null);
    p.catch((e: IpcError) => onError(e));
  };

  const submitLabel = (event: FormEvent) => {
    event.preventDefault();
    const next = label.trim();
    if (next === '' || next.length > 64) return;
    setRenaming(false);
    run(commands.account_set_label({ accountId: account.id, label: next }));
  };

  return (
    <div className="ah-acct" data-account={account.id}>
      <div className="ah-acct-r1">
        <button
          type="button"
          role="radio"
          className="ah-radio"
          aria-checked={account.isDefault}
          aria-label={t('accounts.defaultRadio', { vendor: vendorBadge(account.vendor), account: name })}
          disabled={!account.usableForNew}
          onClick={() => {
            if (!account.isDefault) run(commands.account_set_default({ accountId: account.id }));
          }}
        />
        <span className="ah-acct-name">{name}</span>
        {dir ? (
          <Mono className="ah-acct-dir" ellipsis title={dir}>
            {dir}
          </Mono>
        ) : null}
        <span className="grow" />
        <span className="ah-acct-meta">
          {signedOut ? t('state.error.signIn') : account.signIn === 'unknown' ? t('accounts.signInUnknown') : t('accounts.sessionsNow', { n: account.liveSessions })}
        </span>
        {account.source === 'user' ? (
          <>
            <button
              ref={menuButton}
              type="button"
              className="btn ghost icon"
              aria-label={t('accounts.menu', { account: name })}
              {...popoverTriggerProps(menuOpen, menuId, 'menu')}
              onClick={() => setMenuOpen((o) => !o)}
            >
              <MoreIcon size="sm" />
            </button>
            <Popover open={menuOpen} anchor={menuButton} id={menuId} role="menu" label={t('accounts.menu', { account: name })} onClose={() => setMenuOpen(false)}>
              <button
                type="button"
                role="menuitem"
                onClick={() => {
                  setMenuOpen(false);
                  setLabel(account.label);
                  setRenaming(true);
                }}
              >
                {t('accounts.rename')}
              </button>
              <button
                type="button"
                role="menuitem"
                onClick={() => {
                  setMenuOpen(false);
                  run(commands.account_remove({ accountId: account.id }));
                }}
              >
                {t('accounts.remove')}
              </button>
            </Popover>
          </>
        ) : null}
      </div>
      {renaming ? (
        <form className="ah-rename" onSubmit={submitLabel}>
          <label htmlFor={fieldId} className="ah-sub">
            {t('accounts.labelField')}
          </label>
          <input
            id={fieldId}
            className="input"
            value={label}
            maxLength={64}
            autoFocus
            onChange={(e) => setLabel(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Escape') {
                e.preventDefault();
                e.stopPropagation();
                setRenaming(false);
              }
            }}
          />
          <button type="submit" className="btn">
            {t('accounts.save')}
          </button>
          <button type="button" className="btn ghost" onClick={() => setRenaming(false)}>
            {t('common.cancel')}
          </button>
        </form>
      ) : null}
      {discovered ? (
        <p className="ah-line">
          <span>{t('accounts.discovered')}</span>
          <button type="button" className="btn" onClick={() => run(commands.account_confirm({ accountId: account.id }))}>
            {t('accounts.confirm')}
          </button>
        </p>
      ) : null}
      {signedOut ? (
        <p className="ah-line" data-tone="warn">
          <span>{t('accounts.unavailable.signedOut')}</span>
          <button
            type="button"
            className="btn"
            onClick={() => {
              onError(null);
              commands.account_sign_in({ clientRequestId: newClientRequestId(), accountId: account.id }).then(
                (res) => openSession(res.uid),
                (e: IpcError) => onError(e),
              );
            }}
          >
            {t('action.signIn')}
          </button>
        </p>
      ) : null}
      <QuotaLines quota={account.quota} signedOut={signedOut} />
    </div>
  );
}

function AddAccount({ onError }: { onError: (e: IpcError | null) => void }) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const button = useRef<HTMLButtonElement>(null);
  const id = useId();
  return (
    <>
      <button ref={button} type="button" className="btn" {...popoverTriggerProps(open, id, 'menu')} onClick={() => setOpen((o) => !o)}>
        <PlusIcon size="sm" />
        {t('accounts.add')}
      </button>
      <Popover open={open} anchor={button} id={id} role="menu" label={t('accounts.add')} onClose={() => setOpen(false)}>
        {ADDABLE.map((vendor) => (
          <button
            key={vendor}
            type="button"
            role="menuitem"
            onClick={() => {
              setOpen(false);
              onError(null);
              commands.account_add({ vendor }).catch((e: IpcError) => onError(e));
            }}
          >
            {t('accounts.addFor', { vendor: vendorBadge(vendor) })}
          </button>
        ))}
      </Popover>
    </>
  );
}

function SpendList() {
  const { t, locale } = useI18n();
  const world = useAppStore((s) => s.world);
  const openSession = useAppStore((s) => s.openSession);
  const rows = spendRows(world, localDay(clock.wall()));
  if (rows.length === 0) return <p className="ah-sub ah-pad">{t('accounts.noSessions')}</p>;
  return (
    <ul className="ah-list" aria-label={t('accounts.heading.spend')}>
      {rows.map((r) => {
        const uid = r.uid;
        const team = r.teamKey ? world.teams.get(r.teamKey) : null;
        const name = team ? t('accounts.spend.team', { name: team.name }) : displayName(t, world, r.session, locale);
        const account = r.accountId ? world.accounts.get(r.accountId) : null;
        const tokens = formatTokens(r.tokens, locale);
        const owner = ownerWord(t, r.session.control);
        let value = tokens
          ? t(r.partial ? 'accounts.spend.atLeast' : 'accounts.spend.tokens', { amount: tokens })
          : owner
            ? t('accounts.spend.unknownIn', { owner })
            : t('common.unknown');
        const cost = formatCostMicros(r.costMicros, locale);
        if (cost) value = `${value} · ${cost}`;
        if (r.teamKey) value = `${value} · ${t('accounts.spend.members', { n: r.members })}`;
        return (
          <li key={r.key} className="ah-lrow">
            {uid ? (
              <button type="button" className="ah-lrow-name ah-link" onClick={() => openSession(uid)}>
                {name}
              </button>
            ) : (
              <span className="ah-lrow-name">{name}</span>
            )}
            <span className="ah-lrow-sub">{account ? accountName(t, account) : vendorBadge(r.session.vendor)}</span>
            <span className="ah-lrow-value" data-unknown={tokens ? undefined : 'true'}>
              {value}
            </span>
          </li>
        );
      })}
    </ul>
  );
}

export function AccountsPanel({ headingId }: { headingId: string }) {
  const { t, te } = useI18n();
  const accounts = useAppStore((s) => s.world.accounts);
  const orchestrator = useAppStore((s) => s.world.integrations.orchestrator);
  const [error, setError] = useState<IpcError | null>(null);
  const groups = accountsByVendor(accounts.values());
  const pool = orchestrator !== 'absent' ? orchestratorAccounts(accounts.values()) : [];

  return (
    <section className="ah-panel" aria-labelledby={headingId}>
      <div className="ah-sec">
        <h2 id={headingId}>{t('accounts.heading.accounts')}</h2>
        <span className="grow" />
        <span className="ah-sec-note">{t('accounts.defaultFor')}</span>
      </div>
      <div className="ah-box">
        {groups.length === 0 ? <p className="ah-sub ah-pad">{t('common.loading')}</p> : null}
        {groups.map((g) => (
          <div key={g.vendor} role="radiogroup" aria-label={vendorBadge(g.vendor)} className="ah-vendor">
            <div className="ah-vendor-name badge">{vendorBadge(g.vendor)}</div>
            {g.accounts.map((a) => (
              <AccountRow key={a.id} account={a} onError={setError} />
            ))}
          </div>
        ))}
        <div className="ah-add">
          <AddAccount onError={setError} />
        </div>
        {error ? (
          <p className="ah-line" data-tone="error" role="status">
            <span>{te(error)}</span>
            {error.detail ? <span className="mono ah-detail">{error.detail}</span> : null}
          </p>
        ) : null}
      </div>

      {pool.length > 0 ? (
        <>
          <div className="ah-sec">
            <h2>{t('accounts.heading.orchestrator')}</h2>
          </div>
          <div className="ah-box">
            {pool.map((a) => (
              <div key={a.id} className="ah-acct">
                <div className="ah-acct-r1">
                  <span className="badge">{vendorBadge(a.vendor)}</span>
                  <span className="ah-acct-name">{accountName(t, a)}</span>
                  <span className="grow" />
                  <span className="ah-acct-meta">{t('accounts.sessionsNow', { n: a.liveSessions })}</span>
                </div>
                <QuotaLines quota={a.quota} signedOut={a.signIn === 'signedOut'} />
              </div>
            ))}
          </div>
        </>
      ) : null}

      <div className="ah-sec">
        <h2>{t('accounts.heading.spend')}</h2>
      </div>
      <div className="ah-box">
        <SpendList />
      </div>
    </section>
  );
}
