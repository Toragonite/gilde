/**
 * The Health column of Accounts & health (ux-ia §3.15, §5.9; contract
 * §3.4.10, §4.2.7, §9.7–§9.8).
 *
 * - Memory is the pgid-subtree RSS per session; a team groups under its lead
 *   with its process count; unknown memory stays out of every sum.
 * - A figure above 4 GB per session turns `--warn`.
 * - Leftovers: what each is, why it is a leftover, its memory; `Stop…` and
 *   `Clean up all…` open the Clean-up sheet. `▸` shows the technical facts
 *   (pid, start, the redacted command, the parent chain) on demand — pid stays
 *   hidden until then (ux-ia §9.1 jargon rule).
 */

import { useEffect, useId, useState } from 'react';

import { Mono } from '../components/Mono';
import { Sparkline } from '../components/Sparkline';
import { StateGlyph } from '../components/StateGlyph';
import { ChevronDownIcon, ChevronRightIcon } from '../components/icons';
import { useI18n } from '../i18n/useI18n';
import { clock } from '../lib/clock';
import { commands } from '../lib/commands';
import { formatBytes, formatDateTime, formatDuration } from '../lib/format';
import type { HealthProcessDetailRes } from '../lib/gen/HealthProcessDetailRes';
import type { ProcessRecord } from '../lib/gen/ProcessRecord';
import type { IpcError } from '../lib/ipcError';
import { vendorBadge } from '../lib/vendor';
import { displayName, ownerWord } from '../shell/words';
import { agentMemory } from '../store/healthSlice';
import { appStore, useAppStore } from '../store/useAppStore';
import { HEALTH_ROWS, healthRows, leftoverList, MEM_WARN_BYTES } from './model';
import { REASON_KEY, requestCleanUp } from './StopSheet';

// ---------------------------------------------------------------------------
// Recent memory (memory only, sampled while the screen is open)
// ---------------------------------------------------------------------------

const SAMPLE_MS = 30_000;
const MAX_SAMPLES = 120;
const samples: number[] = [];
let sampler: ReturnType<typeof setInterval> | null = null;
let watchers = 0;

function sample(): void {
  samples.push(agentMemory(appStore.getState().world).bytes);
  if (samples.length > MAX_SAMPLES) samples.splice(0, samples.length - MAX_SAMPLES);
}

function useMemorySamples(): readonly number[] {
  const [, setTick] = useState(0);
  useEffect(() => {
    watchers++;
    sample();
    setTick((n) => n + 1);
    if (!sampler) {
      sampler = setInterval(() => {
        sample();
      }, SAMPLE_MS);
    }
    const tick = setInterval(() => setTick((n) => n + 1), SAMPLE_MS);
    return () => {
      clearInterval(tick);
      watchers--;
      if (watchers === 0 && sampler) {
        clearInterval(sampler);
        sampler = null;
      }
    };
  }, []);
  return samples;
}

/** Tests only. */
export function resetMemorySamplesForTests(): void {
  samples.length = 0;
}

// ---------------------------------------------------------------------------
// Folders macOS denied (§5.8, A38.8): one line per category World reports
// as a `health.folderDenied{category}` notice.
// ---------------------------------------------------------------------------

const FOLDER_CATEGORY = {
  config: 'health.folderCategory.config',
  teams: 'health.folderCategory.teams',
  transcripts: 'health.folderCategory.transcripts',
  projects: 'health.folderCategory.projects',
} as const;

/** The denied folder categories, in World's order, each once (unknown ones are skipped). */
export function deniedFolderCategories(notices: readonly { key: string; params: Record<string, unknown> }[]): (keyof typeof FOLDER_CATEGORY)[] {
  const out: (keyof typeof FOLDER_CATEGORY)[] = [];
  for (const n of notices) {
    if (n.key !== 'health.folderDenied') continue;
    const c = n.params.category;
    if (typeof c === 'string' && c in FOLDER_CATEGORY && !out.includes(c as keyof typeof FOLDER_CATEGORY)) {
      out.push(c as keyof typeof FOLDER_CATEGORY);
    }
  }
  return out;
}

// ---------------------------------------------------------------------------

function LeftoverRow({ p }: { p: ProcessRecord }) {
  const { t, te, locale } = useI18n();
  const [open, setOpen] = useState(false);
  const [detail, setDetail] = useState<HealthProcessDetailRes | null>(null);
  const [error, setError] = useState<IpcError | null>(null);
  const detailId = useId();

  useEffect(() => {
    if (!open || detail) return;
    let live = true;
    commands.health_process_detail({ processKey: p.key }).then(
      (res) => live && setDetail(res),
      (e: IpcError) => live && setError(e),
    );
    return () => {
      live = false;
    };
  }, [open, detail, p.key]);

  const reason = p.leftover ? t(REASON_KEY[p.leftover.reason] ?? 'state.leftover') : t('state.leftover');
  return (
    <li className="ah-left">
      <div className="ah-lrow">
        <StateGlyph state="leftover" />
        <span className="ah-lrow-name">{reason}</span>
        <span className="ah-lrow-sub">{p.vendor ? `${vendorBadge(p.vendor)} · ` : ''}</span>
        <Mono className="ah-lrow-value">{formatBytes(p.rssBytes, locale)}</Mono>
        <button type="button" className="btn" onClick={() => requestCleanUp([p.key])}>
          {t('accounts.stopOne')}
        </button>
        <button
          type="button"
          className="btn ghost icon"
          aria-expanded={open}
          aria-controls={open ? detailId : undefined}
          aria-label={t('accounts.technical')}
          onClick={() => setOpen((o) => !o)}
        >
          {open ? <ChevronDownIcon size="sm" /> : <ChevronRightIcon size="sm" />}
        </button>
      </div>
      {open ? (
        <div id={detailId} className="ah-tech">
          <p className="ah-sub">
            <Mono small>{t('accounts.tech.line', { pid: p.pid, time: formatDateTime((detail?.startedAt ?? p.startSec * 1000), locale) })}</Mono>
          </p>
          {detail ? (
            <>
              <p className="ah-sub">{t('accounts.tech.command')}</p>
              <pre className="mono ah-pre">{detail.commandRedacted.join(' ')}</pre>
              {detail.parentChain.length > 0 ? (
                <>
                  <p className="ah-sub">{t('accounts.tech.parents')}</p>
                  <pre className="mono ah-pre">{detail.parentChain.join(' › ')}</pre>
                </>
              ) : null}
            </>
          ) : error ? (
            <p className="ah-sub" role="status">
              {te(error)}
            </p>
          ) : (
            <p className="ah-sub">{t('common.loading')}</p>
          )}
        </div>
      ) : null}
    </li>
  );
}

function useNow(stepMs: number): number {
  const [now, setNow] = useState(() => clock.wall());
  useEffect(() => {
    const id = setInterval(() => setNow(clock.wall()), stepMs);
    return () => clearInterval(id);
  }, [stepMs]);
  return now;
}

export function HealthPanel({ headingId }: { headingId: string }) {
  const { t, locale } = useI18n();
  const world = useAppStore((s) => s.world);
  const openSession = useAppStore((s) => s.openSession);
  const navigate = useAppStore((s) => s.navigate);
  const [all, setAll] = useState(false);
  const now = useNow(30_000);
  const history = useMemorySamples();

  const mem = agentMemory(world);
  const rows = healthRows(world);
  const shown = all ? rows : rows.slice(0, HEALTH_ROWS);
  const leftovers = leftoverList(world);
  const leftBytes = leftovers.reduce((n, p) => n + p.rssBytes, 0);
  const denied = deniedFolderCategories(world.notices);

  return (
    <section className="ah-panel" aria-labelledby={headingId}>
      <div className="ah-sec">
        <h2 id={headingId}>{t('accounts.heading.health')}</h2>
        <span className="grow" />
        <span className="ah-sec-note">
          {t('accounts.memLine', { size: formatBytes(mem.bytes, locale) })}
          {mem.unknown > 0 ? ` · ${t('accounts.memUnknown', { n: mem.unknown })}` : ''}
        </span>
      </div>
      <div className="ah-box">
        {denied.map((c) => (
          <p key={c} className="ah-line ah-pad" data-tone="warn" role="note">
            {t('health.folderDenied', { category: t(FOLDER_CATEGORY[c]) })}
          </p>
        ))}
        {history.length >= 2 ? (
          <div className="ah-spark">
            <Sparkline values={history} width={360} height={32} label={t('accounts.memRecent')} />
            <span className="ah-sub">
              {t('accounts.memRecent')} · {t('accounts.memWarn')}
            </span>
          </div>
        ) : null}
        <div className="ah-sec ah-sec-inner">
          <h3>{t('accounts.heading.bySession')}</h3>
        </div>
        {rows.length === 0 ? <p className="ah-sub ah-pad">{t('accounts.noSessions')}</p> : null}
        <ul className="ah-list" aria-label={t('accounts.heading.bySession')}>
          {shown.map((r) => {
            const team = r.teamKey ? world.teams.get(r.teamKey) : null;
            const owner = ownerWord(t, r.session.control);
            const name = team ? t('accounts.spend.team', { name: team.name }) : displayName(t, world, r.session, locale);
            const sub = team ? t('accounts.processes', { n: r.processes }) : owner;
            const teamKey = r.teamKey;
            const uid = r.uid;
            return (
              <li key={r.key} className="ah-lrow">
                <StateGlyph state={r.session.activity.state.kind} likely={r.session.activity.confidence === 'likely'} />
                <button
                  type="button"
                  className="ah-lrow-name ah-link"
                  onClick={() => {
                    if (uid) openSession(uid);
                    else if (teamKey) navigate({ screen: 'team', teamKey, mode: 'panes' });
                  }}
                >
                  {name}
                </button>
                <span className="ah-lrow-sub">{sub ?? ''}</span>
                <Mono className="ah-lrow-value" title={r.memory !== null && r.memory > MEM_WARN_BYTES ? t('accounts.memWarn') : undefined}>
                  <span data-tone={r.memory !== null && r.memory > MEM_WARN_BYTES ? 'warn' : undefined}>
                    {r.memory === null ? t('common.unknown') : formatBytes(r.memory, locale)}
                  </span>
                </Mono>
                <Mono className="ah-lrow-time">{r.startedAt !== null ? formatDuration(Math.max(0, now - r.startedAt), locale) : '—'}</Mono>
              </li>
            );
          })}
        </ul>
        {rows.length > HEALTH_ROWS ? (
          <div className="ah-more">
            {!all ? <span className="ah-sub">{t('accounts.more', { n: rows.length - HEALTH_ROWS })}</span> : null}
            <button type="button" className="btn ghost" onClick={() => setAll((a) => !a)}>
              {t(all ? 'accounts.showFewer' : 'accounts.showAll')}
            </button>
          </div>
        ) : null}
      </div>

      <div className="ah-sec">
        <h2>{t('accounts.heading.leftovers')}</h2>
        {leftovers.length > 0 ? <Mono className="ah-sec-note">{`${leftovers.length} · ${formatBytes(leftBytes, locale)}`}</Mono> : null}
        <span className="grow" />
        {leftovers.length > 0 ? (
          <button type="button" className="btn" onClick={() => requestCleanUp(null)}>
            {t('accounts.cleanUpAll')}
          </button>
        ) : null}
      </div>
      <div className="ah-box">
        {leftovers.length === 0 ? <p className="ah-sub ah-pad">{t('accounts.noLeftovers')}</p> : null}
        <ul className="ah-list" aria-label={t('accounts.heading.leftovers')}>
          {leftovers.map((p) => (
            <LeftoverRow key={p.key} p={p} />
          ))}
        </ul>
        <p className="ah-sub ah-pad">{t('health.never')}</p>
      </div>
    </section>
  );
}
