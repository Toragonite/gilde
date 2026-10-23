/**
 * Settings (`⌘,`, ux-ia §6.1, §6.5, §8, §11; contract §4.2.8, §9.6, A7.8,
 * A16.2, A23.5).
 *
 * - General: language, theme, the IDE "Open in IDE" uses.
 * - Attention: notifications about sessions started elsewhere, finished
 *   turns from them, the no-progress threshold, the approval sound, details
 *   in notifications (off by default, F-31), pause (a pause that runs until
 *   the user resumes — `paused_until ≥ PAUSED_UNTIL_RESUME` — reads "until you
 *   resume", A16.2), the macOS permission and a test notification.
 * - Agents: what is installed, and the executable values. Those are shown
 *   read-only and change only through `settings_set_executable` (the core
 *   validates and asks for a native confirmation), never inline (§9.6).
 * - Integrations: the orchestrator line — the only trace of that layer when
 *   it is absent (P8).
 *
 * Ordinary keys go through `settings_set` with a typed patch; `null` leaves a
 * key unchanged. Default accounts are not a setting: they change in
 * Accounts & health (A7.8).
 */

import { useCallback, useEffect, useId, useRef, useState } from 'react';
import type { ReactNode } from 'react';

import { resolveLocale } from '../i18n';
import type { MessageKey } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import { clock } from '../lib/clock';
import { commands } from '../lib/commands';
import { formatRelativeTime } from '../lib/format';
import type { EditorKind } from '../lib/gen/EditorKind';
import type { ExecutableKey } from '../lib/gen/ExecutableKey';
import type { IdeEditorsRes } from '../lib/gen/IdeEditorsRes';
import type { LocaleSetting } from '../lib/gen/LocaleSetting';
import type { PauseUntil } from '../lib/gen/PauseUntil';
import type { Settings } from '../lib/gen/Settings';
import type { SettingsPatch } from '../lib/gen/SettingsPatch';
import type { Theme } from '../lib/gen/Theme';
import type { Vendor } from '../lib/gen/Vendor';
import type { IpcError } from '../lib/ipcError';
import { VENDORS, vendorBadge } from '../lib/vendor';
import { clockTime, PAUSED_UNTIL_RESUME } from '../shell/words';
import { appStore, useAppStore } from '../store/useAppStore';
import { ExecutableDialog } from './ExecutableDialog';
import './settings.css';

const EMPTY: SettingsPatch = { locale: null, theme: null, attention: null, notify: null, ide: null };

/** "Check again" shows progress at most this long (A29.1). */
export const RESCAN_CAP_MS = 15_000;

/** The no-progress choices, minutes (contract §3.5.2: 60..=3600 s). */
export const NO_PROGRESS_MINUTES = [1, 2, 3, 5, 10, 15, 30, 60] as const;

const BINARY_KEY: Readonly<Record<Vendor, ExecutableKey>> = {
  claude: 'agents.claude.binary',
  codex: 'agents.codex.binary',
  cursor: 'agents.cursor.binary',
  agy: 'agents.agy.binary',
};

const BINARY_FIELD = {
  claude: 'claudeBinary',
  codex: 'codexBinary',
  cursor: 'cursorBinary',
  agy: 'agyBinary',
} as const;

const EDITOR_LABEL: Readonly<Record<EditorKind, string>> = {
  vscode: 'VS Code',
  cursor: 'cursor',
};

type SectionId = 'general' | 'attention' | 'agents' | 'integrations';
const SECTIONS: readonly { id: SectionId; key: MessageKey }[] = [
  { id: 'general', key: 'settings.section.general' },
  { id: 'attention', key: 'settings.section.attention' },
  { id: 'agents', key: 'settings.section.agents' },
  { id: 'integrations', key: 'settings.section.integrations' },
];

/** "until you resume" for any value at or above the open-ended pause (A16.2). */
export function pauseState(pausedUntil: number | null): 'on' | 'untilResume' | 'until' {
  if (pausedUntil === null) return 'on';
  return pausedUntil >= PAUSED_UNTIL_RESUME ? 'untilResume' : 'until';
}

function Row({ label, labelId, desc, children }: { label: string; labelId: string; desc?: string | null; children: ReactNode }) {
  return (
    <div className="se-row">
      <div className="se-k">
        <span id={labelId} className="se-label">
          {label}
        </span>
        {desc ? <span className="se-desc">{desc}</span> : null}
      </div>
      <div className="se-v">{children}</div>
    </div>
  );
}

function Seg<V extends string | boolean>({
  labelledBy,
  value,
  options,
  onChange,
  disabled,
}: {
  labelledBy: string;
  value: V | null;
  options: readonly { value: V; label: string }[];
  onChange: (value: V) => void;
  disabled?: boolean;
}) {
  return (
    <div className="seg" role="group" aria-labelledby={labelledBy}>
      {options.map((o) => (
        <button key={String(o.value)} type="button" aria-pressed={o.value === value} disabled={disabled} onClick={() => o.value !== value && onChange(o.value)}>
          {o.label}
        </button>
      ))}
    </div>
  );
}

export function SettingsScreen() {
  const { t, te, locale } = useI18n();
  const uid = useId();
  const id = (s: string) => `${uid}-${s}`;
  const setLocale = useAppStore((s) => s.setLocale);
  const setTheme = useAppStore((s) => s.setTheme);
  const storePaused = useAppStore((s) => s.world.queue.pausedUntil);
  const orchestrator = useAppStore((s) => s.world.integrations.orchestrator);
  const orchestratorTeams = useAppStore((s) => [...s.world.teams.values()].filter((x) => x.source === 'orchestrator').length);
  const orchestratorAccounts = useAppStore((s) => [...s.world.accounts.values()].filter((x) => x.source === 'orchestrator').length);
  const agentsDetected = useAppStore((s) => s.world.boot.agentsDetected);
  const agents = useAppStore((s) => s.world.boot.agents);
  const scannedAt = useAppStore((s) => s.world.boot.agentsScannedAt);

  const [settings, setSettings] = useState<Settings | null>(null);
  const [error, setError] = useState<IpcError | null>(null);
  const [editors, setEditors] = useState<IdeEditorsRes | null>(null);
  const [pauseOverride, setPauseOverride] = useState<{ value: number | null } | null>(null);
  const [line, setLine] = useState<{ section: SectionId; text: string; tone?: 'warn' | 'error' } | null>(null);
  const [exec, setExec] = useState<{ key: ExecutableKey; name: string; current: string } | null>(null);
  /** "Check again" runs until `agentsScannedAt` moves past `from`, at most 15 s (A29.1). */
  const [rescan, setRescan] = useState<{ from: number | null; startedAt: number } | null>(null);
  const sectionRefs = useRef(new Map<SectionId, HTMLElement>());

  const load = useCallback(() => {
    commands.settings_get({}).then(setSettings, (e: IpcError) => setError(e));
  }, []);

  useEffect(() => {
    load();
    commands.ide_editors({}).then(setEditors, () => setEditors({ found: [], chosen: null }));
  }, [load]);

  // A rerun ends when the core reports a newer detection, or after 15 s (the
  // previous rows stay either way).
  useEffect(() => {
    if (!rescan) return;
    if (scannedAt !== null && scannedAt !== rescan.from) {
      setRescan(null);
      return;
    }
    const id = setInterval(() => {
      if (clock.now() - rescan.startedAt >= RESCAN_CAP_MS) setRescan(null);
    }, 250);
    return () => clearInterval(id);
  }, [rescan, scannedAt]);

  const checkAgain = () => {
    if (rescan) return;
    const from = appStore.getState().world.boot.agentsScannedAt;
    setRescan({ from, startedAt: clock.now() });
    commands.agents_rescan({}).catch((e: IpcError) => {
      setRescan(null);
      setLine({ section: 'agents', text: te(e), tone: 'error' });
    });
  };

  useEffect(() => {
    setPauseOverride(null);
  }, [storePaused]);

  const patch = useCallback(
    (p: Partial<SettingsPatch>) => {
      setError(null);
      commands.settings_set({ patch: { ...EMPTY, ...p } }).then(
        (res) => {
          setSettings(res);
          if (p.locale) setLocale(resolveLocale(res.locale));
          if (p.theme) setTheme(res.theme);
        },
        (e: IpcError) => setError(e),
      );
    },
    [setLocale, setTheme],
  );

  const pause = (until: PauseUntil) => {
    commands.notify_pause({ until }).then(
      (res) => setPauseOverride({ value: res.pausedUntil }),
      (e: IpcError) => setLine({ section: 'attention', text: te(e), tone: 'error' }),
    );
  };
  const resume = () => {
    commands.notify_resume({}).then(
      () => setPauseOverride({ value: null }),
      (e: IpcError) => setLine({ section: 'attention', text: te(e), tone: 'error' }),
    );
  };

  const pausedUntil = pauseOverride ? pauseOverride.value : storePaused;
  const ps = pauseState(pausedUntil);
  const pauseText = (() => {
    if (ps === 'on') return t('settings.pause.state.on');
    if (ps === 'untilResume') return t('settings.pause.state.untilResume');
    const at = pausedUntil as number;
    const today = new Date(clock.wall()).toDateString() === new Date(at).toDateString();
    const time = today ? clockTime(at, locale) : `${new Intl.DateTimeFormat(locale, { month: 'short', day: 'numeric' }).format(new Date(at))} ${clockTime(at, locale)}`;
    return t('settings.pause.state.until', { time });
  })();

  const lineFor = (section: SectionId) =>
    line && line.section === section ? (
      <p className="se-line" data-tone={line.tone} role="status">
        {line.text}
      </p>
    ) : null;

  const resetExecutable = (key: ExecutableKey) => {
    commands.settings_set_executable({ key, value: key === 'agents.envPassthrough' ? { kind: 'envNames', names: [] } : { kind: 'path', path: null } }).then(
      (res) => {
        setLine({ section: 'agents', text: t(res.applied ? 'settings.exec.applied' : 'settings.exec.cancelled') });
        if (res.applied) load();
      },
      (e: IpcError) => setLine({ section: 'agents', text: te(e), tone: 'error' }),
    );
  };

  const execRow = (key: ExecutableKey, label: string, value: string | null, labelId: string, extra?: ReactNode) => (
    <Row label={label} labelId={labelId} key={key}>
      <span className="mono se-value" data-auto={value === null ? 'true' : undefined} aria-describedby={labelId}>
        {value ?? t('settings.exec.automatic')}
      </span>
      <button type="button" className="btn" onClick={() => setExec({ key, name: label, current: value ?? '' })}>
        {t('settings.exec.change')}
      </button>
      {value !== null ? (
        <button type="button" className="btn ghost" onClick={() => resetExecutable(key)}>
          {t('settings.exec.reset')}
        </button>
      ) : null}
      {extra}
    </Row>
  );

  const ex = settings?.executable ?? null;
  const vendorStatus = (v: Vendor): string => {
    const o = agents.find((x) => x.vendor === v);
    if (!o || !agentsDetected) return t('settings.agents.unknown');
    if (!o.installed) return t('settings.agents.notInstalled');
    return o.version ? t('settings.agents.installed', { version: o.version }) : t('settings.agents.installedNoVersion');
  };

  return (
    <div className="se">
      <div className="se-head">
        <h1>{t('settings.title')}</h1>
      </div>
      <div className="se-main">
        <nav className="se-nav" aria-label={t('settings.sections')}>
          {SECTIONS.map((s) => (
            <button key={s.id} type="button" className="se-nav-item" onClick={() => sectionRefs.current.get(s.id)?.scrollIntoView?.({ block: 'start' })}>
              {t(s.key)}
            </button>
          ))}
        </nav>
        <div className="se-content">
          {error ? (
            <p className="se-line" data-tone="error" role="status">
              <span>{te(error)}</span>
              {error.detail ? <span className="mono se-detail">{error.detail}</span> : null}
            </p>
          ) : null}
          {!settings && !error ? <p className="se-line">{t('common.loading')}</p> : null}

          <section
            className="se-sec"
            aria-labelledby={id('general')}
            ref={(el) => {
              if (el) sectionRefs.current.set('general', el);
            }}
          >
            <h2 id={id('general')}>{t('settings.section.general')}</h2>
            <Row label={t('settings.language')} labelId={id('lang')}>
              <Seg<LocaleSetting>
                labelledBy={id('lang')}
                value={settings?.locale ?? null}
                disabled={!settings}
                options={[
                  { value: 'system', label: t('settings.language.system') },
                  { value: 'ko', label: t('settings.language.ko') },
                  { value: 'en', label: t('settings.language.en') },
                ]}
                onChange={(v) => patch({ locale: v })}
              />
            </Row>
            <Row label={t('settings.theme')} labelId={id('theme')}>
              <Seg<Theme>
                labelledBy={id('theme')}
                value={settings?.theme ?? null}
                disabled={!settings}
                options={[
                  { value: 'system', label: t('settings.theme.system') },
                  { value: 'light', label: t('settings.theme.light') },
                  { value: 'dark', label: t('settings.theme.dark') },
                ]}
                onChange={(v) => patch({ theme: v })}
              />
            </Row>
            <Row label={t('settings.ide')} labelId={id('ide')} desc={t('settings.ideDesc')}>
              {editors && editors.found.length === 0 ? (
                <span className="se-desc">{t('settings.ide.notFound')}</span>
              ) : (
                <Seg<EditorKind>
                  labelledBy={id('ide')}
                  value={settings?.ide.editor ?? null}
                  disabled={!settings || !editors}
                  options={(editors?.found ?? []).map((e) => ({ value: e.editor, label: EDITOR_LABEL[e.editor] ?? e.editor }))}
                  onChange={(v) => patch({ ide: { editor: v } })}
                />
              )}
              {settings && settings.ide.editor === null && editors && editors.found.length > 0 ? <span className="se-desc">{t('settings.ide.notChosen')}</span> : null}
            </Row>
          </section>

          <section
            className="se-sec"
            aria-labelledby={id('attention')}
            ref={(el) => {
              if (el) sectionRefs.current.set('attention', el);
            }}
          >
            <h2 id={id('attention')}>{t('settings.section.attention')}</h2>
            <Row label={t('settings.notify.watched')} labelId={id('watched')} desc={t('settings.notify.watchedDesc')}>
              <Seg<boolean>
                labelledBy={id('watched')}
                value={settings?.notify.watchedSessions ?? null}
                disabled={!settings}
                options={[
                  { value: true, label: t('settings.on') },
                  { value: false, label: t('settings.off') },
                ]}
                onChange={(v) => patch({ notify: { watchedSessions: v, sound: null, showDetails: null } })}
              />
            </Row>
            <Row label={t('settings.attention.watchedFinished')} labelId={id('finished')} desc={t('settings.attention.watchedFinishedDesc')}>
              <Seg<boolean>
                labelledBy={id('finished')}
                value={settings?.attention.watchedFinishedTurns ?? null}
                disabled={!settings}
                options={[
                  { value: true, label: t('settings.show') },
                  { value: false, label: t('settings.hide') },
                ]}
                onChange={(v) => patch({ attention: { noProgressAfterSecs: null, watchedFinishedTurns: v } })}
              />
            </Row>
            <Row label={t('settings.attention.noProgress')} labelId={id('noprog')} desc={t('settings.attention.noProgressDesc')}>
              <select
                className="input se-select"
                aria-labelledby={id('noprog')}
                disabled={!settings}
                value={settings ? String(settings.attention.noProgressAfterSecs) : ''}
                onChange={(e) => patch({ attention: { noProgressAfterSecs: Number(e.target.value), watchedFinishedTurns: null } })}
              >
                {settings && !NO_PROGRESS_MINUTES.some((m) => m * 60 === settings.attention.noProgressAfterSecs) ? (
                  <option value={String(settings.attention.noProgressAfterSecs)}>{t('settings.minutes', { mins: Math.round(settings.attention.noProgressAfterSecs / 60) })}</option>
                ) : null}
                {NO_PROGRESS_MINUTES.map((m) => (
                  <option key={m} value={String(m * 60)}>
                    {t('settings.minutes', { mins: m })}
                  </option>
                ))}
              </select>
            </Row>
            <Row label={t('settings.notify.sound')} labelId={id('sound')} desc={t('settings.notify.soundDesc')}>
              <Seg<boolean>
                labelledBy={id('sound')}
                value={settings?.notify.sound ?? null}
                disabled={!settings}
                options={[
                  { value: true, label: t('settings.on') },
                  { value: false, label: t('settings.off') },
                ]}
                onChange={(v) => patch({ notify: { watchedSessions: null, sound: v, showDetails: null } })}
              />
            </Row>
            <Row label={t('notify.showDetails')} labelId={id('details')} desc={t('settings.notify.showDetailsDesc')}>
              <Seg<boolean>
                labelledBy={id('details')}
                value={settings?.notify.showDetails ?? null}
                disabled={!settings}
                options={[
                  { value: true, label: t('settings.on') },
                  { value: false, label: t('settings.off') },
                ]}
                onChange={(v) => patch({ notify: { watchedSessions: null, sound: null, showDetails: v } })}
              />
            </Row>
            <Row label={t('settings.pause')} labelId={id('pause')} desc={pauseText}>
              {ps === 'on' ? (
                <div className="se-actions" role="group" aria-labelledby={id('pause')}>
                  <button type="button" className="btn" onClick={() => pause('oneHour')}>
                    {t('settings.pause.oneHour')}
                  </button>
                  <button type="button" className="btn" onClick={() => pause('tomorrowNine')}>
                    {t('settings.pause.tomorrow')}
                  </button>
                  <button type="button" className="btn" onClick={() => pause('untilResume')}>
                    {t('settings.pause.untilResume')}
                  </button>
                </div>
              ) : (
                <button type="button" className="btn" onClick={resume}>
                  {t('settings.pause.resume')}
                </button>
              )}
            </Row>
            <Row label={t('settings.permission')} labelId={id('perm')}>
              <button
                type="button"
                className="btn"
                onClick={() =>
                  commands.notify_permission_request({}).then(
                    (res) => setLine({ section: 'attention', text: t(res.granted ? 'settings.permission.granted' : 'settings.permission.denied'), tone: res.granted ? undefined : 'warn' }),
                    (e: IpcError) => setLine({ section: 'attention', text: te(e), tone: 'error' }),
                  )
                }
              >
                {t('settings.permission.ask')}
              </button>
              <button
                type="button"
                className="btn"
                onClick={() =>
                  commands.notify_test({}).then(
                    (res) => setLine({ section: 'attention', text: t(res.delivered ? 'settings.notify.testDelivered' : 'settings.notify.testBlocked'), tone: res.delivered ? undefined : 'warn' }),
                    (e: IpcError) => setLine({ section: 'attention', text: te(e), tone: 'error' }),
                  )
                }
              >
                {t('settings.notify.test')}
              </button>
            </Row>
            {lineFor('attention')}
          </section>

          <section
            className="se-sec"
            aria-labelledby={id('agents')}
            ref={(el) => {
              if (el) sectionRefs.current.set('agents', el);
            }}
          >
            <h2 id={id('agents')}>{t('settings.section.agents')}</h2>
            <p className="se-desc se-pad">{agentsDetected ? t('settings.agents.desc') : t('newsession.findingAgents')}</p>
            {VENDORS.map((v) => {
              const status = vendorStatus(v);
              const notInstalled = agentsDetected && agents.some((x) => x.vendor === v && !x.installed);
              const value = ex ? ex[BINARY_FIELD[v]] : null;
              return (
                <div key={v} className="se-agent">
                  <div className="se-agent-head">
                    <span className="badge">{vendorBadge(v)}</span>
                    <span className="se-desc">{status}</span>
                  </div>
                  {execRow(BINARY_KEY[v], `${vendorBadge(v)} · ${t('settings.exec.path')}`, value, id(`bin-${v}`))}
                  {notInstalled ? <p className="se-desc se-hint">{t('settings.agents.notInstalledHint', { vendor: vendorBadge(v) })}</p> : null}
                </div>
              );
            })}
            {execRow('sidecar.nodePath', t('settings.exec.nodePath'), ex?.sidecarNodePath ?? null, id('node'))}
            {execRow('paths.override', t('settings.exec.pathsOverride'), ex?.pathsOverride ?? null, id('paths'))}
            {execRow('agents.envPassthrough', t('settings.exec.env'), ex && ex.envPassthrough.length > 0 ? ex.envPassthrough.join(' ') : null, id('env'))}
            <div className="se-actions se-pad">
              <button type="button" className="btn" disabled={rescan !== null} onClick={checkAgain}>
                {t('settings.agents.rescan')}
              </button>
              {rescan ? (
                <span className="se-desc" role="status">
                  {t('settings.agents.rescanning')}
                </span>
              ) : scannedAt !== null && agentsDetected ? (
                <span className="se-desc">{t('settings.agents.checkedAt', { time: formatRelativeTime(scannedAt, locale, Math.max(clock.wall(), scannedAt)) })}</span>
              ) : null}
            </div>
            {lineFor('agents')}
          </section>

          <section
            className="se-sec"
            aria-labelledby={id('integrations')}
            ref={(el) => {
              if (el) sectionRefs.current.set('integrations', el);
            }}
          >
            <h2 id={id('integrations')}>{t('settings.section.integrations')}</h2>
            <p className="se-desc se-pad">
              {orchestrator === 'absent'
                ? t('orchestrator.absent')
                : orchestrator === 'present'
                  ? t('settings.orchestrator.connected', { teams: orchestratorTeams, accounts: orchestratorAccounts })
                  : t('settings.orchestrator.notRunning')}
            </p>
          </section>
        </div>
      </div>
      {exec ? (
        <ExecutableDialog
          settingKey={exec.key}
          name={exec.name}
          current={exec.current}
          onClose={(applied) => {
            setExec(null);
            if (applied) {
              setLine({ section: 'agents', text: t('settings.exec.applied') });
              load();
            }
          }}
        />
      ) : null}
    </div>
  );
}
