/**
 * New session (`⌘N`, ux-ia §3.12, §5.8; contract §4.2.2, A23.5, A24.4, A26.2).
 *
 * The zero-choice path is `⌘N`, optionally a task, `⏎`: every chip is
 * preselected from the core's `newsession_options` defaults (per project:
 * last vendor, model, mode and location; the account rule of §5.8), and the
 * project from the sheet's context (`openSheet('newSession', { projectId })`),
 * else the project open in Main, else the first one. `⏎` pressed before the
 * options arrive starts as soon as they do.
 *
 *  - `⏎` starts, `⇧⏎` adds a line, nothing fires during Korean composition;
 *    `⌘⏎` starts from anywhere in the sheet (it is not destructive).
 *  - `⌥1`…`⌥8` open the chips in order (project, agent, account, model,
 *    where, permissions, mode, team), also from the prompt; `↑↓ ⏎` pick in a
 *    menu; `Esc` closes the menu, a second `Esc` cancels the sheet.
 *  - A default that changed is said once, under the chips, with its undo
 *    ("Personal is at 91% … so Work is used. [Use Personal]"); the sheet never
 *    asks.
 *  - A folder picked here is a project at once (A24.4): the start needs no
 *    prior session in it.
 *
 * Sheet convention (A23.5): no props; it reads its context from the store and
 * closes with `closeSheet()`. The typed task survives Cancel (memory only) and
 * is cleared by a successful start.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { ReactNode } from 'react';

import { Kbd } from '../components/Badge';
import { ChevronDownIcon, UsersIcon } from '../components/icons';
import { Sheet } from '../components/Sheet';
import { useHotkeys } from '../hooks/useHotkeys';
import type { HotkeyBinding } from '../hooks/useHotkeys';
import type { MessageKey } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import type { AccountId } from '../lib/gen/AccountId';
import type { ControlMode } from '../lib/gen/ControlMode';
import type { LocationId } from '../lib/gen/LocationId';
import type { NewSessionOptions } from '../lib/gen/NewSessionOptions';
import type { PermissionMode } from '../lib/gen/PermissionMode';
import type { Project } from '../lib/gen/Project';
import type { ProjectId } from '../lib/gen/ProjectId';
import type { Quota } from '../lib/gen/Quota';
import { newClientRequestId } from '../lib/ids';
import type { IpcError } from '../lib/ipcError';
import { vendorBadge } from '../lib/vendor';
import { clockTime } from '../shell/words';
import { appStore, useAppStore } from '../store/useAppStore';
import type { WorldState } from '../store/worldSlice';
import { ChipMenu } from './ChipMenu';
import type { ChipMenuItem } from './ChipMenu';
import * as M from './model';
import './newsession.css';

const CHIP_LABEL: Readonly<Record<M.ChipId, MessageKey>> = {
  project: 'newsession.chip.project',
  vendor: 'newsession.chip.vendor',
  account: 'newsession.chip.account',
  model: 'newsession.chip.model',
  where: 'newsession.chip.where',
  permissions: 'newsession.chip.permissions',
  mode: 'newsession.chip.mode',
  team: 'newsession.chip.team',
};

const PERM_LABEL: Readonly<Record<PermissionMode, MessageKey>> = {
  default: 'newsession.perm.default',
  acceptEdits: 'newsession.perm.acceptEdits',
  plan: 'newsession.perm.plan',
  dontAsk: 'newsession.perm.dontAsk',
  auto: 'newsession.perm.auto',
  bypassPermissions: 'newsession.perm.bypassPermissions',
};

const MODE_LABEL: Readonly<Record<ControlMode, MessageKey>> = {
  conversation: 'mode.conversation',
  terminal: 'mode.terminal',
  turnBased: 'mode.turnBased',
};

const MODE_DESC: Readonly<Record<ControlMode, MessageKey>> = {
  conversation: 'newsession.mode.conversationDesc',
  terminal: 'newsession.mode.terminalDesc',
  turnBased: 'newsession.mode.turnBasedDesc',
};

const WINDOW_LABEL = {
  fiveHour: 'accounts.window.fiveHour',
  week: 'accounts.window.week',
  other: 'accounts.window.other',
} as const;

/** The typed task, kept across Cancel for this window (memory only). */
let draft = '';

/** Tests only. */
export function resetNewSessionDraftForTests(): void {
  draft = '';
}

export function NewSessionSheet() {
  const open = useAppStore((s) => s.ui.sheet === 'newSession');
  return open ? <NewSessionBody /> : null;
}

function liveAt(world: WorldState, locationId: LocationId | null): number {
  if (!locationId) return 0;
  let n = 0;
  for (const s of world.sessions.values()) if (s.locationId === locationId && s.control.kind !== 'notRunning' && !s.mergedInto) n++;
  return n;
}

function NewSessionBody() {
  const { t, td, te, locale } = useI18n();
  const closeSheet = useAppStore((s) => s.closeSheet);
  const openSession = useAppStore((s) => s.openSession);
  const projects = useAppStore((s) => s.world.projects);
  const storeAccounts = useAppStore((s) => s.world.accounts);
  const agentsDetected = useAppStore((s) => s.world.boot.agentsDetected);

  const [projectId, setProjectId] = useState<ProjectId | null>(() => {
    const { world, ui } = appStore.getState();
    return M.preselectProject(world, ui.view, ui.split.focused, ui.sheetContext?.projectId);
  });
  const [picked, setPicked] = useState<Project | null>(null);
  const [prompt, setPrompt] = useState(() => draft);
  const [picks, setPicks] = useState<M.Picks>({});
  const [options, setOptions] = useState<{ key: string; value: NewSessionOptions } | null>(null);
  const [optionsError, setOptionsError] = useState<IpcError | null>(null);
  const [openChip, setOpenChip] = useState<M.ChipId | null>(null);
  const [startWanted, setStartWanted] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<IpcError | null>(null);
  const [switchUndone, setSwitchUndone] = useState(false);
  const [slugEdit, setSlugEdit] = useState<string | null>(null);
  const [fallback] = useState(() => M.fallbackSlug(new Date(), M.randomHex4()));

  const bodyRef = useRef<HTMLDivElement>(null);
  const promptRef = useRef<HTMLTextAreaElement>(null);
  const chipRefs = useRef(new Map<M.ChipId, HTMLButtonElement>());
  const inFlight = useRef(false);
  const promptNow = useRef(prompt);
  promptNow.current = prompt;

  // The task survives Cancel and any other close, but not a start.
  useEffect(
    () => () => {
      if (!inFlight.current) draft = promptNow.current;
    },
    [],
  );

  const project = (projectId && projects.get(projectId)) || (picked && picked.id === projectId ? picked : null);
  const vendorPick = picks.vendor ?? null;
  const optionsKey = `${projectId ?? ''}|${vendorPick ?? ''}`;
  const ready = options !== null && options.key === optionsKey ? options.value : null;

  // Options for the project and the picked vendor (models, modes, permissions
  // and accounts depend on the vendor). Asked again once agents are detected.
  useEffect(() => {
    if (!projectId) return;
    let live = true;
    const key = `${projectId}|${vendorPick ?? ''}`;
    setOptionsError(null);
    commands.newsession_options({ projectId, vendor: vendorPick }).then(
      (value) => {
        if (live) setOptions({ key, value });
      },
      (e: IpcError) => {
        if (!live) return;
        setOptionsError(e);
        setStartWanted(false);
      },
    );
    return () => {
      live = false;
    };
  }, [projectId, vendorPick, agentsDetected]);

  const r = useMemo(() => M.resolve(ready, picks), [ready, picks]);
  const slug = M.effectiveSlug(prompt, slugEdit, fallback);
  const blocker = M.blockerOf(projectId, ready, r, slug, prompt);

  const fail = useCallback((e: unknown) => setError(e as IpcError), []);

  const pickFolder = useCallback(
    async (thenStart: boolean) => {
      setOpenChip(null);
      try {
        const res = await commands.project_pick({});
        if (!res.project) return;
        setPicked(res.project);
        setProjectId(res.project.id);
        setPicks((p) => ({ ...(p.vendor ? { vendor: p.vendor } : {}), ...(p.team ? { team: true } : {}) }));
        setSlugEdit(null);
        if (thenStart) setStartWanted(true);
      } catch (e) {
        fail(e);
      }
    },
    [fail],
  );

  const start = useCallback(() => {
    if (busy || inFlight.current) return;
    setError(null);
    if (!projectId) {
      void pickFolder(true);
      return;
    }
    setStartWanted(true);
  }, [busy, projectId, pickFolder]);

  // Start once the options for this project and vendor are here.
  useEffect(() => {
    if (!startWanted || inFlight.current || !projectId) return;
    if (!ready) return;
    if (blocker) {
      setStartWanted(false);
      return;
    }
    inFlight.current = true;
    setBusy(true);
    const req = M.buildStartReq(newClientRequestId(), projectId, r, slug, prompt);
    commands.session_start(req).then(
      (res) => {
        draft = '';
        closeSheet();
        openSession(res.uid);
      },
      (e: IpcError) => {
        inFlight.current = false;
        setBusy(false);
        setStartWanted(false);
        setError(e);
      },
    );
  }, [startWanted, ready, blocker, projectId, r, slug, prompt, closeSheet, openSession]);

  const signIn = useCallback(
    async (accountId: AccountId) => {
      setOpenChip(null);
      try {
        const res = await commands.account_sign_in({ clientRequestId: newClientRequestId(), accountId });
        closeSheet();
        openSession(res.uid);
      } catch (e) {
        fail(e);
      }
    },
    [closeSheet, openSession, fail],
  );

  const cancel = useCallback(() => {
    closeSheet();
  }, [closeSheet]);

  // ---- chips -------------------------------------------------------------

  const chipDisabled = (id: M.ChipId): boolean => {
    switch (id) {
      case 'project':
        return false;
      case 'vendor':
        return !ready || M.installedVendors(ready).length === 0;
      case 'account':
        return r.accounts.length === 0;
      case 'model':
        return !ready || !r.vendor;
      case 'where':
        return !ready;
      case 'permissions':
        return !ready || ready.permissionModes.length <= 1;
      case 'mode':
        return !ready || r.team || r.modes.length <= 1;
      case 'team':
        return !r.teamAllowed;
    }
  };

  const closeMenu = useCallback((focusChip: M.ChipId | null) => {
    setOpenChip(null);
    if (focusChip) chipRefs.current.get(focusChip)?.focus();
  }, []);

  const toggleChip = (id: M.ChipId) => {
    if (chipDisabled(id)) return;
    if (id === 'team') {
      setPicks((p) => ({ ...p, team: !r.team }));
      setOpenChip(null);
      return;
    }
    setOpenChip((cur) => (cur === id ? null : id));
  };

  const chipBindings: HotkeyBinding[] = M.CHIP_ORDER.map((id, i) => ({
    id: `newSession.chip.${id}`,
    combo: `alt+${i + 1}`,
    allowInTextEntry: true,
    run: () => toggleChip(id),
  }));
  useHotkeys(chipBindings, { scope: 'N', element: bodyRef });
  useHotkeys([{ id: 'newSession.start', combo: 'enter', allowInTextEntry: true, run: () => start() }], {
    scope: 'N',
    element: promptRef,
    requireFocus: true,
  });

  // ---- labels --------------------------------------------------------------

  const quotaText = (quota: Quota | null | undefined): string | null => {
    if (!quota || quota.windows.length === 0) return null;
    const parts: string[] = [];
    for (const w of quota.windows) {
      if (w.usedPct === null || w.usedPct < 0 || w.usedPct > 100) continue;
      parts.push(t('newsession.quotaWindow', { window: w.label ?? t(WINDOW_LABEL[w.kind]), pct: w.usedPct }));
      if (w.usedPct >= M.NEAR_LIMIT_PCT && w.resetsAt !== null) parts.push(t('accounts.resets', { time: clockTime(w.resetsAt, locale) }));
    }
    return parts.length > 0 ? parts.join(' · ') : null;
  };

  /** The core's "Default" label reads in the UI language. */
  const shown = (label: string): string => (label === 'Default' || label === '' ? t('account.default') : label);

  const accountLabel = (id: AccountId | null): string => {
    if (!id) return '—';
    const option = r.accounts.find((a) => a.accountId === id);
    const raw = option?.label ?? storeAccounts.get(id)?.label;
    const label = raw === undefined ? t('account.unknown') : shown(raw);
    const pct = M.maxUsedPct(storeAccounts.get(id)?.quota);
    return pct === null ? label : `${label} · ${pct}%`;
  };

  const locationOption = (locationId: LocationId | null) =>
    ready?.locations.find((l) => 'locationId' in l.location && l.location.locationId === locationId) ?? null;

  const whereLabel = (where: M.Where): string => {
    switch (where.kind) {
      case 'here': {
        const branch = locationOption(where.locationId)?.branch ?? null;
        return branch ? t('newsession.here', { branch }) : t('newsession.hereFolder');
      }
      case 'newWorktree':
        return t('newsession.newWorktree', { slug: slug || '…' });
      case 'existingWorktree': {
        const o = locationOption(where.locationId);
        return t('newsession.existingWorktree', { name: o?.worktreeName ?? o?.branch ?? '…' });
      }
    }
  };

  const modelLabel = (id: string | null): string => {
    if (id === null) return t('newsession.model.default');
    return ready?.models.find((m) => m.id === id)?.label ?? id;
  };

  const chipValue = (id: M.ChipId): string => {
    switch (id) {
      case 'project':
        return project?.name ?? t('newsession.chooseFolder');
      case 'vendor':
        return r.vendor ? vendorBadge(r.vendor) : '…';
      case 'account':
        return ready ? accountLabel(r.accountId) : '…';
      case 'model':
        return ready ? modelLabel(r.model) : '…';
      case 'where':
        return ready ? whereLabel(r.where) : '…';
      case 'permissions':
        return r.permissionMode ? t(PERM_LABEL[r.permissionMode]) : '…';
      case 'mode':
        return r.controlMode ? t(MODE_LABEL[r.controlMode]) : '…';
      case 'team':
        return t('newsession.teamChip', { state: t(r.team ? 'common.on' : 'common.off') });
    }
  };

  // ---- menus -----------------------------------------------------------------

  const menuItems = (id: M.ChipId): ChipMenuItem[] => {
    switch (id) {
      case 'project': {
        const items: ChipMenuItem[] = M.orderedProjects({ projects }).map((p) => ({
          key: p.id,
          label: p.name,
          sub: p.liveSessions > 0 ? t('newsession.projectRunning', { n: p.liveSessions }) : null,
          checked: p.id === projectId,
          onSelect: () => {
            if (p.id !== projectId) {
              setProjectId(p.id);
              setPicks((x) => ({ ...(x.vendor ? { vendor: x.vendor } : {}), ...(x.team ? { team: true } : {}) }));
              setSlugEdit(null);
            }
            closeMenu('project');
          },
        }));
        if (picked && !projects.has(picked.id)) {
          items.unshift({ key: picked.id, label: picked.name, checked: picked.id === projectId, onSelect: () => closeMenu('project') });
        }
        items.push({ key: '__pick', label: t('newsession.chooseFolder'), onSelect: () => void pickFolder(false) });
        return items;
      }
      case 'vendor':
        return (ready?.vendors ?? [])
          .filter((v) => v.installed)
          .map((v) => ({
            key: v.vendor,
            label: vendorBadge(v.vendor),
            sub: v.version,
            checked: v.vendor === r.vendor,
            onSelect: () => {
              if (v.vendor !== r.vendor) setPicks((p) => M.picksForVendor(p, v.vendor));
              closeMenu('vendor');
            },
          }));
      case 'account':
        return r.accounts.map((a) => ({
          key: a.accountId,
          label: shown(a.label),
          sub:
            a.signIn === 'signedOut'
              ? t('state.error.signIn')
              : [quotaText(storeAccounts.get(a.accountId)?.quota), a.accountId === ready?.defaults.accountId ? t('newsession.recommended') : null]
                  .filter(Boolean)
                  .join(' · ') || null,
          checked: a.accountId === r.accountId,
          onSelect: () => {
            setPicks((p) => ({ ...p, accountId: a.accountId }));
            setSwitchUndone(true);
            closeMenu('account');
          },
        }));
      case 'model': {
        const ids: (string | null)[] = [null, ...(ready?.models ?? []).map((m) => m.id)];
        return ids.map((m) => ({
          key: m ?? '__default',
          label: modelLabel(m),
          checked: m === r.model,
          onSelect: () => {
            setPicks((p) => ({ ...p, model: m }));
            closeMenu('model');
          },
        }));
      }
      case 'where': {
        const items: ChipMenuItem[] = [];
        for (const l of ready?.locations ?? []) {
          const loc = l.location;
          if (loc.kind === 'here') {
            const w: M.Where = { kind: 'here', locationId: loc.locationId };
            const n = liveAt(appStore.getState().world, loc.locationId);
            items.push({
              key: `here-${loc.locationId ?? ''}`,
              label: whereLabel(w),
              sub: n > 0 ? t('newsession.otherHere', { n }) : null,
              checked: r.where.kind === 'here' && r.where.locationId === loc.locationId,
              onSelect: () => {
                setPicks((p) => ({ ...p, where: w }));
                closeMenu('where');
              },
            });
          } else if (loc.kind === 'existingWorktree') {
            const w: M.Where = { kind: 'existingWorktree', locationId: loc.locationId };
            items.push({
              key: `wt-${loc.locationId}`,
              label: whereLabel(w),
              sub: l.branch,
              checked: r.where.kind === 'existingWorktree' && r.where.locationId === loc.locationId,
              onSelect: () => {
                setPicks((p) => ({ ...p, where: w }));
                closeMenu('where');
              },
            });
          }
        }
        if (!items.some((i) => i.key.startsWith('here-'))) {
          const w: M.Where = { kind: 'here', locationId: null };
          items.unshift({
            key: 'here-',
            label: t('newsession.hereFolder'),
            checked: r.where.kind === 'here',
            onSelect: () => {
              setPicks((p) => ({ ...p, where: w }));
              closeMenu('where');
            },
          });
        }
        items.push({
          key: 'new',
          label: t('newsession.newWorktree', { slug: slug || '…' }),
          sub: slug ? t('newsession.newWorktreeBranch', { slug }) : null,
          checked: r.where.kind === 'newWorktree',
          onSelect: () => {
            setPicks((p) => ({ ...p, where: { kind: 'newWorktree' } }));
            closeMenu('where');
          },
        });
        return items;
      }
      case 'permissions':
        return (ready?.permissionModes ?? []).map((m) => ({
          key: m,
          label: t(PERM_LABEL[m]),
          checked: m === r.permissionMode,
          onSelect: () => {
            setPicks((p) => ({ ...p, permissionMode: m }));
            closeMenu('permissions');
          },
        }));
      case 'mode':
        return r.modes.map((m) => ({
          key: m,
          label: t(MODE_LABEL[m]),
          sub: t(MODE_DESC[m]),
          checked: m === r.controlMode,
          onSelect: () => {
            setPicks((p) => ({ ...p, controlMode: m }));
            closeMenu('mode');
          },
        }));
      case 'team':
        return [];
    }
  };

  const slugField =
    openChip === 'where' ? (
      <div className="ns-slug">
        <label className="ns-slug-label" htmlFor="ns-slug">
          {t('newsession.worktreeName')}
        </label>
        <span className="ns-slug-prefix mono" aria-hidden="true">
          wt/
        </span>
        <input
          id="ns-slug"
          className="input mono ns-slug-input"
          value={slug}
          spellCheck={false}
          autoComplete="off"
          aria-invalid={!M.SLUG_RE.test(slug)}
          onFocus={() => setPicks((p) => ({ ...p, where: { kind: 'newWorktree' } }))}
          onChange={(e) => setSlugEdit(e.target.value.toLowerCase())}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.nativeEvent.isComposing) {
              e.preventDefault();
              closeMenu('where');
            }
          }}
        />
      </div>
    ) : null;

  // ---- lines under the chips -------------------------------------------------------

  const lines: { key: string; tone?: 'warn' | 'error'; node: ReactNode }[] = [];
  const notices = ready?.notices ?? [];
  if (!agentsDetected && !notices.some((n) => n.key === 'newsession.findingAgents')) {
    lines.push({ key: 'finding', node: t('newsession.findingAgents') });
  }
  for (const n of notices) {
    if (n.key === 'accounts.switched') {
      if (switchUndone || picks.accountId) continue;
      const from = M.switchedFrom(n, r.accounts);
      lines.push({
        key: n.key,
        node: (
          <>
            <span>{td(n.key, { ...M.noticeParams(n, locale), a: shown(String(n.params.a ?? '')), b: shown(String(n.params.b ?? '')) })}</span>
            {from ? (
              <button
                type="button"
                className="btn ghost ns-line-action"
                onClick={() => {
                  setPicks((p) => ({ ...p, accountId: from }));
                  setSwitchUndone(true);
                }}
              >
                {t('newsession.useAccount', { account: shown(String(n.params.a ?? '')) })}
              </button>
            ) : null}
          </>
        ),
      });
    } else {
      lines.push({ key: n.key, node: td(n.key, M.noticeParams(n, locale)) });
    }
  }
  if (r.team && ready) lines.push({ key: 'team', node: t(ready.teamMode === 'panes' ? 'team.paneHint' : 'team.inProcess') });
  if (r.permissionMode === 'bypassPermissions') {
    lines.push({ key: 'bypass', tone: 'warn', node: `${t('newsession.bypassWarn')}${r.team ? ` ${t('team.bypassMayPrompt')}` : ''}` });
  }
  const chosenAccount = r.accounts.find((a) => a.accountId === r.accountId) ?? null;
  if (chosenAccount && chosenAccount.signIn === 'signedOut') {
    lines.push({
      key: 'signIn',
      tone: 'warn',
      node: (
        <>
          <span>{t('newsession.signInNeeded', { account: shown(chosenAccount.label) })}</span>
          <button type="button" className="btn ghost ns-line-action" onClick={() => void signIn(chosenAccount.accountId)}>
            {t('action.signIn')}
          </button>
        </>
      ),
    });
  }
  if (blocker === 'noProject') {
    lines.push({
      key: 'noProject',
      node: (
        <>
          <span>{t('newsession.noProject')}</span>
          <button type="button" className="btn ghost ns-line-action" onClick={() => void pickFolder(false)}>
            {t('newsession.chooseFolder')}
          </button>
        </>
      ),
    });
  } else if (blocker === 'noAgents' && agentsDetected) lines.push({ key: 'noAgents', tone: 'warn', node: t('newsession.noAgents') });
  else if (blocker === 'noAccount') lines.push({ key: 'noAccount', tone: 'warn', node: t('newsession.noAccount') });
  else if (blocker === 'slugInvalid') lines.push({ key: 'slug', tone: 'warn', node: t('newsession.slugInvalid') });
  else if (blocker === 'promptTooLong') lines.push({ key: 'long', tone: 'warn', node: t('newsession.promptTooLong') });
  for (const [key, e] of [
    ['optionsError', optionsError],
    ['error', error],
  ] as const) {
    if (!e) continue;
    const signInFix = e.code === 'REFUSED' && e.params.reason === 'signInNeeded' && r.accountId;
    lines.push({
      key,
      tone: 'error',
      node: (
        <>
          <span>{te(e)}</span>
          {e.detail ? <span className="mono ns-detail">{e.detail}</span> : null}
          {signInFix ? (
            <button type="button" className="btn ghost ns-line-action" onClick={() => void signIn(r.accountId as AccountId)}>
              {t('action.signIn')}
            </button>
          ) : null}
        </>
      ),
    });
  }
  if (busy || (startWanted && !ready && projectId)) lines.push({ key: 'busy', node: t('newsession.starting') });

  const startDisabled = busy || (blocker !== null && blocker !== 'loading' && blocker !== 'noProject');
  const menuId = 'ns-menu';

  return (
    <Sheet
      open
      size="lg"
      title={t('action.newSession')}
      onCancel={cancel}
      onConfirm={start}
      className="ns-sheet"
      actions={
        <>
          <span className="ns-keys" aria-hidden="true">
            {t('newsession.chipKeys')}
          </span>
          <button type="button" className="btn" onClick={cancel}>
            {t('common.cancel')}
          </button>
          <button
            type="button"
            className="btn primary"
            onClick={start}
            disabled={startDisabled}
            aria-keyshortcuts="Enter Meta+Enter"
            data-ns-start=""
          >
            {t('newsession.start')} <Kbd>⏎</Kbd>
          </button>
        </>
      }
    >
      <div ref={bodyRef} className="ns-body">
        <textarea
          ref={promptRef}
          className="textarea ns-prompt"
          aria-label={t('newsession.prompt')}
          placeholder={t('newsession.placeholder')}
          rows={3}
          value={prompt}
          data-autofocus=""
          onChange={(e) => setPrompt(e.target.value)}
        />
        <div className="ns-chips" role="group" aria-label={t('newsession.chips')}>
          {M.CHIP_ORDER.map((id, i) => {
            const value = chipValue(id);
            const isTeam = id === 'team';
            return (
              <button
                key={id}
                ref={(el) => {
                  if (el) chipRefs.current.set(id, el);
                  else chipRefs.current.delete(id);
                }}
                type="button"
                className="ns-chip"
                data-chip={id}
                disabled={chipDisabled(id)}
                aria-haspopup={isTeam ? undefined : 'menu'}
                aria-expanded={isTeam ? undefined : openChip === id}
                aria-controls={!isTeam && openChip === id ? menuId : undefined}
                aria-pressed={isTeam ? r.team : undefined}
                aria-keyshortcuts={`Alt+${i + 1}`}
                onClick={() => toggleChip(id)}
              >
                <span className="ns-chip-key" aria-hidden="true">
                  ⌥{i + 1}
                </span>
                {isTeam ? null : <span className="g-sr-only">{`${t('newsession.chipPrefix', { chip: t(CHIP_LABEL[id]) })} `}</span>}
                {isTeam ? <UsersIcon size="sm" /> : null}
                <span className="ns-chip-value">{value}</span>
                {isTeam ? null : <ChevronDownIcon size="sm" />}
              </button>
            );
          })}
        </div>
        {openChip && openChip !== 'team' ? (
          <ChipMenu
            key={openChip}
            id={menuId}
            label={t(CHIP_LABEL[openChip])}
            items={menuItems(openChip)}
            onClose={() => closeMenu(openChip)}
            extra={slugField}
          />
        ) : null}
        {lines.length > 0 ? (
          <div className="ns-lines" role="status">
            {lines.map((l) => (
              <p key={l.key} className="ns-line" data-tone={l.tone}>
                {l.node}
              </p>
            ))}
          </div>
        ) : null}
      </div>
    </Sheet>
  );
}
