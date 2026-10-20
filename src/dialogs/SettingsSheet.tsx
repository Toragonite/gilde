/**
 * Settings sheet — ⌘, (T26; DESIGN §10-3, §10-6, §9-8).
 *
 * Everything here edits `config.json` through `app_settings_set(patch)`, which
 * deep-merges the subtree the sheet sends and answers with the whole config, so
 * the form never has to guess what the backend actually stored: what comes back
 * is what is on disk. A `null` clears a key back to its built-in default.
 *
 * The defaults this sheet must not quietly contradict (DECISIONS §M1 세부 기본값):
 * `rawPtyLog` off, `autoKillOverHardCeiling` off, `idleTimeoutMinutes` 0 — the
 * app warns, it does not kill on its own. Each of those says so under the field.
 *
 * The diagnostics block is the "one click, everything a bug report needs" view
 * of `app_diagnostics()`. That command is registered by the integration wave; a
 * backend that does not answer it yet renders as an honest one-line note rather
 * than as an error. The status bar's "Not connected — details" opens the sheet
 * focused on it (`ui.settingsFocus === 'diagnostics'`), which also runs it.
 *
 * Notifications: one toggle, `config.notifications.system` (default on), held
 * in the store (`ui.systemNotifications`) because the health slice reads it.
 * When macOS refused the permission this run (`lib/notify.ts`), a muted line
 * under it says where to turn it back on.
 *
 * Preview build (`src/features.ts`): Appearance, Notifications and Diagnostics only. The
 * sections that configure the experimental lanes — Terminal, Memory and
 * supervision, Scan intervals, Agent binaries, Launch overrides, the terminal
 * font size and the signature buttons — appear only with the lanes on.
 */

import { useCallback, useEffect, useMemo, useState, useSyncExternalStore } from 'react';
import type { ReactNode } from 'react';

import { Mono } from '../components/Mono';
import { experimentalLanes } from '../features';
import { Sheet } from '../components/Sheet';
import { useI18n } from '../i18n/useI18n';
import { LOCALES } from '../i18n';
import type { MessageKey } from '../i18n';
import { LANGUAGE_NAME } from '../shell/TitleBar';
import { cmd, isUnavailable, asGildeError } from '../lib/commands';
import { notifyPermission, subscribeNotifyPermission } from '../lib/notify';
import type {
  AgentKindKey,
  GildeErrorPayload,
  AppConfig,
  Diagnostics,
  LocalePreference,
  PathsInfo,
  PromptMode,
  ThemePreference,
} from '../lib/ipc';
import { useAppStore } from '../store/useAppStore';
import './dialogs.css';

/** The agents `config.agents.binaries` carries (DESIGN §10-3). */
export const BINARY_KINDS: readonly AgentKindKey[] = [
  'claude',
  'codex',
  'cursorAgent',
  'agy',
  'gemini',
];

export const THEMES: readonly ThemePreference[] = ['system', 'light', 'dark'];

/** The language choices: follow the system (the default, F1), or one language. */
export const LOCALE_CHOICES: readonly LocalePreference[] = ['system', ...LOCALES];

/** DESIGN §6-6. `argv` is the M1 default; `typeAfterReady` is the escape hatch. */
export const PROMPT_MODES: readonly PromptMode[] = ['argv', 'typeAfterReady', 'none'];

const THEME_LABEL: Record<ThemePreference, MessageKey> = {
  system: 'app.theme.system',
  light: 'app.theme.light',
  dark: 'app.theme.dark',
};

// ---------------------------------------------------------------------------
// Pure helpers (unit tested)
// ---------------------------------------------------------------------------

/** Build the `{ agents: { binaries: { … } } }` patch for one binary override. */
export function binaryPatch(kind: AgentKindKey, path: string): Record<string, unknown> {
  const trimmed = path.trim();
  return { agents: { binaries: { [kind]: trimmed.length > 0 ? trimmed : null } } };
}

/** Build the `{ agents: { launchOverrides: { … } } }` patch for one agent. */
export function launchOverridePatch(
  kind: AgentKindKey,
  patch: { promptMode?: PromptMode; args?: string[] },
): Record<string, unknown> {
  return { agents: { launchOverrides: { [kind]: patch } } };
}

/**
 * Extra launch arguments are edited as one line and stored as an array, exactly
 * as `LaunchOverride.args` declares it. Whitespace separates; nothing is quoted,
 * because an argument that needs quoting belongs in the signatures file.
 */
export function splitArgs(line: string): string[] {
  return line.split(/\s+/).filter((part) => part.length > 0);
}

export function joinArgs(args: readonly string[] | undefined): string {
  return (args ?? []).join(' ');
}

/** A number field commits only a finite, in-range value; anything else reverts. */
export function parseBounded(text: string, min: number, max: number): number | null {
  const value = Number(text);
  if (!Number.isFinite(value)) return null;
  if (value < min || value > max) return null;
  return value;
}

// ---------------------------------------------------------------------------
// Small form primitives
// ---------------------------------------------------------------------------

interface RowProps {
  label: string;
  hint?: string;
  children: ReactNode;
}

function Row({ label, hint, children }: RowProps) {
  return (
    <>
      <div className="set-row">
        <span className="set-label">{label}</span>
        <div className="set-control">{children}</div>
      </div>
      {hint ? (
        <div className="set-row">
          <span />
          <span className="set-hint">{hint}</span>
        </div>
      ) : null}
    </>
  );
}

interface ToggleProps {
  value: boolean;
  disabled?: boolean;
  label: string;
  onChange: (value: boolean) => void;
}

function Toggle({ value, disabled, label, onChange }: ToggleProps) {
  const { t } = useI18n();
  return (
    <div className="seg" role="group" aria-label={label}>
      <button
        type="button"
        aria-pressed={value}
        disabled={disabled}
        onClick={() => onChange(true)}
      >
        {t('common.on')}
      </button>
      <button
        type="button"
        aria-pressed={!value}
        disabled={disabled}
        onClick={() => onChange(false)}
      >
        {t('common.off')}
      </button>
    </div>
  );
}

interface NumberFieldProps {
  value: number;
  min: number;
  max: number;
  step?: number;
  label: string;
  onCommit: (value: number) => void;
}

function NumberField({ value, min, max, step, label, onCommit }: NumberFieldProps) {
  const [text, setText] = useState(() => String(value));
  useEffect(() => setText(String(value)), [value]);

  const commit = () => {
    const parsed = parseBounded(text, min, max);
    if (parsed === null || parsed === value) {
      setText(String(value));
      return;
    }
    onCommit(parsed);
  };

  return (
    <input
      className="input set-num"
      type="number"
      inputMode="numeric"
      min={min}
      max={max}
      step={step}
      value={text}
      aria-label={label}
      onChange={(event) => setText(event.target.value)}
      onBlur={commit}
      onKeyDown={(event) => {
        if (event.key === 'Enter') commit();
      }}
    />
  );
}

interface TextFieldProps {
  value: string;
  label: string;
  placeholder?: string;
  onCommit: (value: string) => void;
}

function TextField({ value, label, placeholder, onCommit }: TextFieldProps) {
  const [text, setText] = useState(value);
  useEffect(() => setText(value), [value]);

  const commit = () => {
    if (text === value) return;
    onCommit(text);
  };

  return (
    <input
      className="input set-path"
      type="text"
      spellCheck={false}
      value={text}
      aria-label={label}
      placeholder={placeholder}
      onChange={(event) => setText(event.target.value)}
      onBlur={commit}
      onKeyDown={(event) => {
        if (event.key === 'Enter') commit();
      }}
    />
  );
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

export interface DiagnosticsViewProps {
  diagnostics: Diagnostics;
}

export function DiagnosticsView({ diagnostics }: DiagnosticsViewProps) {
  const { t } = useI18n();
  const d = diagnostics;
  return (
    <div className="set-diag">
      <span className="k">{t('settings.diag.pathSource')}</span>
      <span className="v">{d.pathSource}</span>
      <span className="k">{t('settings.diag.pathFingerprint')}</span>
      <span className="v">{d.pathFingerprint}</span>
      <span className="k">{t('settings.diag.path')}</span>
      <span className="v">{d.resolvedPath.join('\n')}</span>
      <span className="k">{t('settings.diag.agents')}</span>
      <span className="v">
        {d.agents
          .map((agent) => `${agent.kind} ${agent.version ?? '-'} ${agent.absPath ?? '-'}`)
          .join('\n')}
      </span>
      <span className="k">{t('settings.diag.sidecar')}</span>
      <span className="v">
        {[d.sidecar.state, d.sidecar.nodeSource, d.sidecar.nodePath ?? '-'].join(' ')}
      </span>
      <span className="k">{t('settings.diag.db')}</span>
      <span className="v">{`${d.db.path} v${d.db.schemaVersion}`}</span>
      <span className="k">{t('settings.diag.pressureSource')}</span>
      <span className="v">{d.memoryPressureSource}</span>
      <span className="k">{t('settings.diag.instance')}</span>
      <span className="v">{`${d.appInstanceId} ${d.bootId}`}</span>
      <span className="k">{t('settings.diag.dirs')}</span>
      <span className="v">{[d.appDataDir, d.worktreeRoot, d.dispatchRoot].join('\n')}</span>
    </div>
  );
}

// ---------------------------------------------------------------------------
// The form (portal-free, so the static tests can mount it)
// ---------------------------------------------------------------------------

export interface SettingsFormProps {
  config: AppConfig | null;
  paths: PathsInfo | null;
  diagnostics: Diagnostics | null;
  diagnosticsUnavailable: boolean;
  error: GildeErrorPayload | null;
  onPatch: (patch: Record<string, unknown>) => void;
  onShowDiagnostics: () => void;
  onRevealSignatures: () => void;
  onReloadSignatures: () => void;
  /** Bring the diagnostics block into view (the status bar's "details"). */
  focusDiagnostics?: boolean;
}

export function SettingsForm({
  config,
  paths,
  diagnostics,
  diagnosticsUnavailable,
  error,
  onPatch,
  onShowDiagnostics,
  onRevealSignatures,
  onReloadSignatures,
  focusDiagnostics = false,
}: SettingsFormProps) {
  const { t, td } = useI18n();
  const setLocale = useAppStore((s) => s.setLocale);
  const setTheme = useAppStore((s) => s.setTheme);
  const theme = useAppStore((s) => s.ui.theme);
  const localePref = useAppStore((s) => s.ui.localePref);
  const systemNotifications = useAppStore((s) => s.ui.systemNotifications);
  const setSystemNotifications = useAppStore((s) => s.setSystemNotifications);
  const permission = useSyncExternalStore(subscribeNotifyPermission, notifyPermission, notifyPermission);
  const lanes = experimentalLanes();

  useEffect(() => {
    if (!focusDiagnostics || typeof document === 'undefined') return;
    document.getElementById('settings-diagnostics')?.scrollIntoView({ block: 'start' });
  }, [focusDiagnostics, config]);

  const diagnosticsBlock = (
    <>
      <div className="set-sec" id="settings-diagnostics">
        {t('settings.section.diagnostics')}
      </div>
      <div className="set-group">
        <div className="row gap-8">
          {lanes ? (
            <>
              <button
                type="button"
                className="btn"
                disabled={!paths?.signaturesFile}
                title={paths?.signaturesFile}
                onClick={onRevealSignatures}
              >
                {t('settings.openSignatures')}
              </button>
              <button type="button" className="btn" onClick={onReloadSignatures}>
                {t('settings.reloadSignatures')}
              </button>
            </>
          ) : null}
          <button type="button" className="btn" onClick={onShowDiagnostics}>
            {t('settings.showDiagnostics')}
          </button>
        </div>
        {diagnosticsUnavailable ? (
          <span className="set-hint">
            {td('error.commandUnavailable', { command: 'app_diagnostics' })}
          </span>
        ) : null}
        {diagnostics ? <DiagnosticsView diagnostics={diagnostics} /> : null}
        {paths ? (
          <Mono small ellipsis className="muted" title={paths.configFile}>
            {paths.configFile}
          </Mono>
        ) : null}
      </div>
    </>
  );

  if (!config) {
    return (
      <div className="set">
        <span className="set-hint">{t('boot.settingsUnavailable')}</span>
        {diagnosticsBlock}
        {error ? <span className="set-issue">{td(error.key, error.params)}</span> : null}
      </div>
    );
  }

  const terminal = config.terminal;

  return (
    <div className="set">
      {/* -- appearance ----------------------------------------------------- */}
      <div className="set-sec">{t('settings.section.appearance')}</div>
      <div className="set-group">
        <Row label={t('settings.locale')}>
          <div className="seg" role="group" aria-label={t('settings.locale')}>
            {LOCALE_CHOICES.map((candidate) => (
              <button
                key={candidate}
                type="button"
                aria-pressed={localePref === candidate}
                onClick={() => setLocale(candidate)}
                lang={candidate === 'system' ? undefined : candidate}
              >
                {candidate === 'system' ? t('settings.locale.system') : LANGUAGE_NAME[candidate]}
              </button>
            ))}
          </div>
        </Row>

        <Row label={t('settings.theme')}>
          <div className="seg" role="group" aria-label={t('settings.theme')}>
            {THEMES.map((candidate) => (
              <button
                key={candidate}
                type="button"
                aria-pressed={theme === candidate}
                onClick={() => setTheme(candidate)}
              >
                {t(THEME_LABEL[candidate])}
              </button>
            ))}
          </div>
        </Row>

        {lanes ? (
          <Row label={t('settings.fontSize')}>
            <NumberField
              value={terminal.fontSize}
              min={9}
              max={24}
              step={0.5}
              label={t('settings.fontSize')}
              onCommit={(value) => onPatch({ terminal: { fontSize: value } })}
            />
          </Row>
        ) : null}
      </div>

      {/* -- notifications -------------------------------------------------- */}
      <div className="set-sec">{t('settings.section.notifications')}</div>
      <div className="set-group">
        <Row label={t('settings.systemNotifications')} hint={t('settings.systemNotifications.hint')}>
          <Toggle
            value={systemNotifications}
            label={t('settings.systemNotifications')}
            onChange={setSystemNotifications}
          />
        </Row>
        {permission === 'denied' ? (
          <div className="set-row">
            <span />
            <span className="set-hint muted" data-testid="notify-denied">
              {t('settings.systemNotifications.denied')}
            </span>
          </div>
        ) : null}
      </div>

      {lanes ? (
        <LaneSettings config={config} onPatch={onPatch} />
      ) : null}

      {diagnosticsBlock}

      {error ? <span className="set-issue">{td(error.key, error.params)}</span> : null}
      {error?.detail ? <pre className="mono s11 muted empty-detail">{error.detail}</pre> : null}
    </div>
  );
}

/**
 * The settings of the experimental lanes (terminal, supervision, scan
 * intervals, agent binaries, launch overrides). Rendered only with the lanes on.
 */
function LaneSettings({
  config,
  onPatch,
}: {
  config: AppConfig;
  onPatch: (patch: Record<string, unknown>) => void;
}) {
  const { t } = useI18n();
  const supervision = config.supervision;
  return (
    <>
      {/* -- terminal ------------------------------------------------------- */}
      <div className="set-sec">{t('settings.section.terminal')}</div>
      <div className="set-group">
        <Row label={t('settings.rawPtyLog')} hint={t('settings.rawPtyLog.hint')}>
          <Toggle
            value={supervision.rawPtyLog}
            label={t('settings.rawPtyLog')}
            onChange={(value) => onPatch({ supervision: { rawPtyLog: value } })}
          />
        </Row>
      </div>

      {/* -- memory and supervision ----------------------------------------- */}
      <div className="set-sec">{t('settings.section.memory')}</div>
      <div className="set-group">
        <Row label={t('settings.softRss')}>
          <NumberField
            value={supervision.softRssMiB}
            min={128}
            max={65536}
            label={t('settings.softRss')}
            onCommit={(value) => onPatch({ supervision: { softRssMiB: value } })}
          />
        </Row>
        <Row label={t('settings.hardRss')}>
          <NumberField
            value={supervision.hardRssMiB}
            min={128}
            max={131072}
            label={t('settings.hardRss')}
            onCommit={(value) => onPatch({ supervision: { hardRssMiB: value } })}
          />
        </Row>
        <Row label={t('settings.warnPct')}>
          <NumberField
            value={supervision.globalWarnPct}
            min={1}
            max={100}
            label={t('settings.warnPct')}
            onCommit={(value) => onPatch({ supervision: { globalWarnPct: value } })}
          />
        </Row>
        <Row label={t('settings.criticalPct')}>
          <NumberField
            value={supervision.globalCriticalPct}
            min={1}
            max={100}
            label={t('settings.criticalPct')}
            onCommit={(value) => onPatch({ supervision: { globalCriticalPct: value } })}
          />
        </Row>
        <Row label={t('settings.autoKill')} hint={t('settings.autoKill.hint')}>
          <Toggle
            value={supervision.autoKillOverHardCeiling}
            label={t('settings.autoKill')}
            onChange={(value) => onPatch({ supervision: { autoKillOverHardCeiling: value } })}
          />
        </Row>
        <Row label={t('settings.idleTimeout')} hint={t('settings.idleTimeout.hint')}>
          <NumberField
            value={supervision.idleTimeoutMinutes}
            min={0}
            max={1440}
            label={t('settings.idleTimeout')}
            onCommit={(value) => onPatch({ supervision: { idleTimeoutMinutes: value } })}
          />
        </Row>
      </div>

      {/* -- scan intervals -------------------------------------------------- */}
      <div className="set-sec">{t('settings.section.scan')}</div>
      <div className="set-group">
        <Row label={t('settings.scanActive')}>
          <NumberField
            value={supervision.scanIntervalSecondsActive}
            min={1}
            max={120}
            label={t('settings.scanActive')}
            onCommit={(value) => onPatch({ supervision: { scanIntervalSecondsActive: value } })}
          />
        </Row>
        <Row label={t('settings.scanBackground')}>
          <NumberField
            value={supervision.scanIntervalSecondsBackground}
            min={1}
            max={600}
            label={t('settings.scanBackground')}
            onCommit={(value) => onPatch({ supervision: { scanIntervalSecondsBackground: value } })}
          />
        </Row>
        <Row label={t('settings.minAgeBulk')} hint={t('settings.minAgeBulk.hint')}>
          <NumberField
            value={supervision.minAgeSecondsForBulk}
            min={0}
            max={3600}
            label={t('settings.minAgeBulk')}
            onCommit={(value) => onPatch({ supervision: { minAgeSecondsForBulk: value } })}
          />
        </Row>
      </div>

      {/* -- agent binaries -------------------------------------------------- */}
      <div className="set-sec">{t('settings.section.binaries')}</div>
      <div className="set-group">
        <span className="set-hint">{t('settings.binaries.hint')}</span>
        {BINARY_KINDS.map((kind) => (
          <Row key={kind} label={t(`settings.binary.${kind}` as MessageKey)}>
            <TextField
              value={config.agents.binaries[kind] ?? ''}
              label={t(`settings.binary.${kind}` as MessageKey)}
              placeholder={t('settings.binary.placeholder')}
              onCommit={(value) => onPatch(binaryPatch(kind, value))}
            />
          </Row>
        ))}
      </div>

      {/* -- launch overrides ------------------------------------------------ */}
      <div className="set-sec">{t('settings.section.launch')}</div>
      <div className="set-group">
        <span className="set-hint">{t('settings.launch.hint')}</span>
        {BINARY_KINDS.map((kind) => {
          const override = config.agents.launchOverrides[kind] ?? {};
          return (
            <Row key={kind} label={t(`settings.binary.${kind}` as MessageKey)}>
              <select
                className="input"
                value={override.promptMode ?? 'argv'}
                aria-label={t('settings.launch.promptMode')}
                onChange={(event) =>
                  onPatch(
                    launchOverridePatch(kind, { promptMode: event.target.value as PromptMode }),
                  )
                }
              >
                {PROMPT_MODES.map((mode) => (
                  <option key={mode} value={mode}>
                    {t(`settings.promptMode.${mode}` as MessageKey)}
                  </option>
                ))}
              </select>
              <TextField
                value={joinArgs(override.args)}
                label={t('settings.launch.args')}
                placeholder={t('settings.launch.argsPlaceholder')}
                onCommit={(value) =>
                  onPatch(launchOverridePatch(kind, { args: splitArgs(value) }))
                }
              />
            </Row>
          );
        })}
      </div>

    </>
  );
}

// ---------------------------------------------------------------------------
// The sheet
// ---------------------------------------------------------------------------

export function SettingsSheet() {
  const { t } = useI18n();
  const open = useAppStore((s) => s.ui.settingsOpen);
  const focus = useAppStore((s) => s.ui.settingsFocus);
  const setSettingsOpen = useAppStore((s) => s.setSettingsOpen);
  const pushToast = useAppStore((s) => s.pushToast);

  const [config, setConfig] = useState<AppConfig | null>(null);
  const [paths, setPaths] = useState<PathsInfo | null>(null);
  const [diagnostics, setDiagnostics] = useState<Diagnostics | null>(null);
  const [diagnosticsUnavailable, setDiagnosticsUnavailable] = useState(false);
  const [error, setError] = useState<GildeErrorPayload | null>(null);

  useEffect(() => {
    if (!open) return;
    let live = true;
    void (async () => {
      try {
        const next = await cmd.appSettingsGet();
        if (live) setConfig(next);
      } catch (e) {
        if (!isUnavailable(e) && live) setError(asGildeError(e));
      }
      try {
        const info = await cmd.appBootstrap();
        if (live) setPaths(info.paths);
      } catch (e) {
        if (!isUnavailable(e)) console.warn('[gilde] app_bootstrap failed', e);
      }
    })();
    return () => {
      live = false;
    };
  }, [open]);

  const onPatch = useCallback((patch: Record<string, unknown>) => {
    setError(null);
    void cmd
      .appSettingsSet(patch)
      .then((next) => setConfig(next))
      .catch((e) => setError(asGildeError(e)));
  }, []);

  const onShowDiagnostics = useCallback(() => {
    setError(null);
    void cmd
      .appDiagnostics()
      .then((next) => {
        setDiagnostics(next);
        setDiagnosticsUnavailable(false);
      })
      .catch((e) => {
        if (isUnavailable(e)) setDiagnosticsUnavailable(true);
        else setError(asGildeError(e));
      });
  }, []);

  // "Not connected — details" in the status bar: open on the diagnostics, run them.
  useEffect(() => {
    if (open && focus === 'diagnostics') onShowDiagnostics();
  }, [open, focus, onShowDiagnostics]);

  const onRevealSignatures = useCallback(() => {
    const file = paths?.signaturesFile;
    if (!file) return;
    void cmd.appReveal(file).catch((e) => setError(asGildeError(e)));
  }, [paths]);

  const onReloadSignatures = useCallback(() => {
    setError(null);
    void cmd
      .agentsReloadSignatures()
      .then((report) =>
        pushToast({
          level: report.warnings.length > 0 ? 'warn' : 'success',
          key: 'settings.signaturesReloaded',
          params: { agents: report.agents, warnings: report.warnings.length },
          detail: report.warnings.join('\n') || null,
          actionId: null,
          undoToken: null,
          ttlMs: 5000,
        }),
      )
      .catch((e) => setError(asGildeError(e)));
  }, [pushToast]);

  const close = useCallback(() => setSettingsOpen(false), [setSettingsOpen]);

  const body = useMemo(
    () => (
      <SettingsForm
        config={config}
        paths={paths}
        diagnostics={diagnostics}
        diagnosticsUnavailable={diagnosticsUnavailable}
        error={error}
        onPatch={onPatch}
        onShowDiagnostics={onShowDiagnostics}
        onRevealSignatures={onRevealSignatures}
        onReloadSignatures={onReloadSignatures}
        focusDiagnostics={focus === 'diagnostics'}
      />
    ),
    [
      focus,
      config,
      paths,
      diagnostics,
      diagnosticsUnavailable,
      error,
      onPatch,
      onShowDiagnostics,
      onRevealSignatures,
      onReloadSignatures,
    ],
  );

  if (!open) return null;

  return (
    <Sheet
      open={open}
      title={t('settings.title')}
      onClose={close}
      actions={
        <button type="button" className="btn" onClick={close}>
          {t('common.close')}
        </button>
      }
    >
      {body}
    </Sheet>
  );
}
