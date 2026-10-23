/**
 * History (`⇧⌘F`, ux-ia §3.14; contract §4.2.8, A23.5).
 *
 * - Search titles and first prompts; filters for project, agent, time,
 *   account and where it ran. Live sessions are in the list too, marked
 *   "running", so nobody has to know which list a session is in.
 * - `⏎` on a row: Continue in Gilde (resume); a session alive elsewhere reads
 *   "Continue a copy in Gilde" (fork); one that runs in Gilde opens. `⌘↓`
 *   reads it in the Session screen. cursor and agy rows are "title only".
 * - The preview pane reads the first prompt and the last reply on demand
 *   (`history_preview`, content, memory only — never stored or logged).
 * - Cold start: while the core is still indexing titles the list says how far
 *   it got ("Searching titles… 120 of 412 loaded.") with a 1 px bar and asks
 *   again every 1.5 s, keeping the selection.
 * - Below 1000 px the preview pane is dropped; `⇥` on a row opens the same
 *   preview as a popover.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent } from 'react';

import { Kbd } from '../components/Badge';
import { Popover } from '../components/Popover';
import { StateGlyph } from '../components/StateGlyph';
import { SearchIcon } from '../components/icons';
import { useHotkeys } from '../hooks/useHotkeys';
import { useRovingTabIndex } from '../hooks/useRovingTabIndex';
import { useI18n } from '../i18n/useI18n';
import { clock } from '../lib/clock';
import { commands } from '../lib/commands';
import type { AccountId } from '../lib/gen/AccountId';
import type { HistoryCoverage } from '../lib/gen/HistoryCoverage';
import type { HistoryCursor } from '../lib/gen/HistoryCursor';
import type { HistoryFilters } from '../lib/gen/HistoryFilters';
import type { HistoryPreviewRes } from '../lib/gen/HistoryPreviewRes';
import type { HistoryRow } from '../lib/gen/HistoryRow';
import type { Origin } from '../lib/gen/Origin';
import type { ProjectId } from '../lib/gen/ProjectId';
import type { QueryId } from '../lib/gen/QueryId';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { Vendor } from '../lib/gen/Vendor';
import { newClientRequestId } from '../lib/ids';
import type { IpcError } from '../lib/ipcError';
import { VENDORS, vendorBadge } from '../lib/vendor';
import { orderedProjects } from '../newsession/model';
import { appStore, useAppStore } from '../store/useAppStore';
import { sessionOf } from '../store/worldSlice';
import * as M from './model';
import './history.css';

const NARROW_WIDTH = 1000;
const PREVIEW_CACHE = 50;
const DEBOUNCE_MS = 150;

type PreviewState = { res: HistoryPreviewRes } | { error: IpcError };

function useNarrow(): boolean {
  const read = () => (typeof window === 'undefined' ? false : window.innerWidth < NARROW_WIDTH);
  const [narrow, setNarrow] = useState(read);
  useEffect(() => {
    const onResize = () => setNarrow(read());
    window.addEventListener('resize', onResize);
    return () => window.removeEventListener('resize', onResize);
  }, []);
  return narrow;
}

function useDebounced<T>(value: T, ms: number): T {
  const [v, setV] = useState(value);
  useEffect(() => {
    const id = setTimeout(() => setV(value), ms);
    return () => clearTimeout(id);
  }, [value, ms]);
  return v;
}

interface SelectProps {
  id: string;
  label: string;
  value: string;
  options: readonly { value: string; label: string }[];
  onChange: (value: string) => void;
}

function FilterSelect({ id, label, value, options, onChange }: SelectProps) {
  return (
    <span className="hi-filter">
      <label htmlFor={id}>{label}</label>
      <select id={id} className="input hi-select" value={value} onChange={(e) => onChange(e.target.value)}>
        {options.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label}
          </option>
        ))}
      </select>
    </span>
  );
}

export function HistoryScreen() {
  const { t, te, locale } = useI18n();
  const projects = useAppStore((s) => s.world.projects);
  const accounts = useAppStore((s) => s.world.accounts);
  const sessions = useAppStore((s) => s.world.sessions);
  const truncated = useAppStore((s) => s.world.sources.some((x) => x.truncated));
  const discovering = useAppStore((s) => !s.world.boot.firstDiscoveryDone);
  const orchestrator = useAppStore((s) => s.ui.features.orchestrator);
  const openSession = useAppStore((s) => s.openSession);
  const narrow = useNarrow();

  const [text, setText] = useState('');
  const [projectId, setProjectId] = useState<ProjectId | null>(null);
  const [vendor, setVendor] = useState<Vendor | null>(null);
  const [when, setWhen] = useState<M.When>('any');
  const [accountId, setAccountId] = useState<AccountId | null>(null);
  const [origin, setOrigin] = useState<Origin | null>(null);
  const debounced = useDebounced(text, DEBOUNCE_MS);

  const [rows, setRows] = useState<HistoryRow[]>([]);
  const [next, setNext] = useState<HistoryCursor | null>(null);
  const [coverage, setCoverage] = useState<HistoryCoverage | null>(null);
  const [pages, setPages] = useState(0);
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState<IpcError | null>(null);
  const [tick, setTick] = useState(0);
  const [selected, setSelected] = useState<SessionUid | null>(null);
  const [previews, setPreviews] = useState<ReadonlyMap<SessionUid, PreviewState>>(new Map());
  const [actionError, setActionError] = useState<{ uid: SessionUid; error: IpcError } | null>(null);
  const [popoverFor, setPopoverFor] = useState<SessionUid | null>(null);

  const latest = useRef<QueryId | null>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const optionRefs = useRef(new Map<SessionUid, HTMLDivElement>());
  const popoverAnchor = useRef<HTMLElement | null>(null);

  const filters = useMemo<HistoryFilters>(
    () => ({ projectId, vendor, from: M.fromOf(when, clock.wall()), to: null, accountId, origin }),
    [projectId, vendor, when, accountId, origin],
  );

  // The first page, again whenever the text or a filter changes, and on the
  // cold-start poll.
  useEffect(() => {
    const queryId = M.newQueryId();
    latest.current = queryId;
    commands.history_query({ queryId, text: M.queryText(debounced), filters, cursor: null, limit: M.PAGE }).then(
      (res) => {
        if (res.superseded || latest.current !== queryId) return;
        setRows(res.rows);
        setNext(res.nextCursor);
        setCoverage(res.coverage);
        setPages(1);
        setLoaded(true);
        setError(null);
      },
      (e: IpcError) => {
        if (latest.current !== queryId) return;
        setLoaded(true);
        setError(e);
      },
    );
  }, [debounced, filters, tick]);

  const indexing = coverage !== null && coverage.titlesLoaded < coverage.total;
  useEffect(() => {
    if (!indexing || pages > 1) return;
    const id = setTimeout(() => setTick((n) => n + 1), M.COLD_START_POLL_MS);
    return () => clearTimeout(id);
  }, [indexing, coverage, pages]);

  const loadMore = useCallback(() => {
    if (!next) return;
    const queryId = M.newQueryId();
    latest.current = queryId;
    commands.history_query({ queryId, text: M.queryText(debounced), filters, cursor: next, limit: M.PAGE }).then(
      (res) => {
        if (res.superseded || latest.current !== queryId) return;
        setRows((r) => {
          const seen = new Set(r.map((x) => x.uid));
          return [...r, ...res.rows.filter((x) => !seen.has(x.uid))];
        });
        setNext(res.nextCursor);
        setCoverage(res.coverage);
        setPages((p) => p + 1);
      },
      (e: IpcError) => {
        if (latest.current === queryId) setError(e);
      },
    );
  }, [next, debounced, filters]);

  // Keep the selection on its row; fall back to the first one.
  const index = rows.findIndex((r) => r.uid === selected);
  useEffect(() => {
    if (rows.length === 0) {
      if (selected !== null) setSelected(null);
      return;
    }
    if (index < 0) setSelected((rows[0] as HistoryRow).uid);
  }, [rows, index, selected]);

  const roving = useRovingTabIndex(rows.length, {
    initialIndex: Math.max(0, index),
    onActiveChange: (i) => {
      const row = rows[i];
      if (row) setSelected(row.uid);
    },
  });
  const { setActiveIndex } = roving;
  useEffect(() => {
    if (index >= 0 && roving.activeIndex !== index) setActiveIndex(index);
  }, [index, roving.activeIndex, setActiveIndex]);

  const selectedRow = index >= 0 ? (rows[index] as HistoryRow) : null;

  // The preview of the selected row (content, memory only).
  useEffect(() => {
    if (!selectedRow || selectedRow.titleOnly || previews.has(selectedRow.uid)) return;
    const uid = selectedRow.uid;
    let live = true;
    const id = setTimeout(() => {
      commands.history_preview({ uid }).then(
        (res) => live && setPreviews((m) => cap(new Map(m).set(uid, { res }))),
        (e: IpcError) => live && setPreviews((m) => cap(new Map(m).set(uid, { error: e }))),
      );
    }, 120);
    return () => {
      live = false;
      clearTimeout(id);
    };
  }, [selectedRow, previews]);

  const act = useCallback(
    (row: HistoryRow, force?: 'fork') => {
      const s = sessionOf(appStore.getState().world, row.uid);
      const action: M.RowAction = force ? { kind: force } : M.rowAction(row, s);
      setActionError(null);
      if (action.kind === 'none') return;
      if (action.kind === 'open') {
        openSession(s ? s.uid : row.uid);
        return;
      }
      commands
        .session_continue({ clientRequestId: newClientRequestId(), uid: row.uid, how: { kind: action.kind }, controlMode: null, model: null, permissionMode: null })
        .then(
          (res) => openSession(res.uid),
          (e: IpcError) => setActionError({ uid: row.uid, error: e }),
        );
    },
    [openSession],
  );

  const read = useCallback((row: HistoryRow) => openSession(row.uid), [openSession]);

  const openFolder = useCallback((row: HistoryRow) => {
    setActionError(null);
    commands.ide_reveal({ target: { kind: 'location', uid: row.uid } }).catch((e: IpcError) => setActionError({ uid: row.uid, error: e }));
  }, []);

  useHotkeys(
    [
      { id: 'history.continue', combo: 'enter', run: () => selectedRow && act(selectedRow) },
      { id: 'history.read', combo: 'mod+arrowdown', run: () => selectedRow && read(selectedRow) },
    ],
    { scope: 'H', element: listRef, requireFocus: true },
  );

  useEffect(() => {
    searchRef.current?.focus();
  }, []);

  const onOptionKeyDown = (event: ReactKeyboardEvent<HTMLElement>, row: HistoryRow, base: (e: ReactKeyboardEvent<HTMLElement>) => void) => {
    if (narrow && event.code === 'Tab' && !event.shiftKey && !event.metaKey && !event.altKey) {
      event.preventDefault();
      popoverAnchor.current = optionRefs.current.get(row.uid) ?? null;
      setPopoverFor(row.uid);
      return;
    }
    base(event);
  };

  // ---- labels ----------------------------------------------------------------

  const now = clock.wall();
  const groups = M.groupRows(rows, now);
  const filtered = text.trim() !== '' || projectId !== null || vendor !== null || when !== 'any' || accountId !== null || origin !== null;
  const all = t('history.filter.all');

  const projectName = (id: ProjectId | null) => (id ? (projects.get(id)?.name ?? null) : null);
  const ownerOf = (row: HistoryRow): string => {
    const s = sessions.get(row.uid);
    if (s && s.control.kind === 'inGilde') return t('ownership.inGilde');
    return t(M.ORIGIN_KEY[row.origin] ?? 'origin.unknown');
  };

  const rowSub = (row: HistoryRow): string =>
    [projectName(row.projectId), vendorBadge(row.vendor), ownerOf(row), M.rowWhen(t, row, locale, now), row.titleOnly ? t('history.titleOnly') : null]
      .filter((x): x is string => !!x)
      .join(' · ');

  const preview = (row: HistoryRow) => {
    const state = previews.get(row.uid);
    const s = sessions.get(row.uid) ?? null;
    const action = M.rowAction(row, s);
    const live = row.running || (s !== null && s.control.kind !== 'notRunning');
    const p = state && 'res' in state ? state.res : null;
    const continuedIn = p?.continuedIn ?? row.continuedIn;
    const successor = continuedIn ? (rows.find((r) => r.uid === continuedIn) ?? null) : null;
    const successorTitle = continuedIn ? (successor ? M.rowTitle(t, successor, locale) : (sessions.get(continuedIn)?.title ?? t('history.untitled'))) : null;
    const err = actionError && actionError.uid === row.uid ? actionError.error : null;
    const offerFork = err !== null && err.code === 'SESSION_LIVE_ELSEWHERE';
    const meta1 = [projectName(row.projectId), vendorBadge(row.vendor), s && s.control.kind === 'inGilde' ? t('ownership.inGilde') : t('history.ranIn', { owner: t(M.ORIGIN_KEY[row.origin] ?? 'origin.unknown') })]
      .filter((x): x is string => !!x)
      .join(' · ');
    const turns = p?.turns ?? null;
    const meta2 = [M.rowWhen(t, row, locale, now), turns !== null ? t('history.turns', { n: turns }) : null, live ? t('history.running') : t('state.ended')]
      .filter((x): x is string => !!x)
      .join(' · ');
    return (
      <div className="hi-prev">
        <h2 className="hi-prev-title">{M.rowTitle(t, row, locale)}</h2>
        <p className="hi-prev-meta">{meta1}</p>
        <p className="hi-prev-meta">{meta2}</p>
        {row.titleOnly ? (
          <p className="hi-prev-note">{t('history.noContent')}</p>
        ) : state && 'error' in state ? (
          <p className="hi-prev-note" role="status">
            {te(state.error)}
          </p>
        ) : p ? (
          <>
            {p.firstPrompt ? (
              <div className="hi-prev-block">
                <h3>{t('history.first')}</h3>
                <p className="hi-prev-text">{p.firstPrompt}</p>
              </div>
            ) : null}
            {p.lastReply ? (
              <div className="hi-prev-block">
                <h3>{t('history.last')}</h3>
                <p className="hi-prev-text">{p.lastReply}</p>
              </div>
            ) : null}
          </>
        ) : (
          <p className="hi-prev-note">{t('common.loading')}</p>
        )}
        {continuedIn && successorTitle ? (
          <p className="hi-prev-meta">
            <button type="button" className="hi-link" onClick={() => (successor ? setSelected(successor.uid) : openSession(continuedIn))}>
              {t('history.continuedAs', { title: successorTitle })}
            </button>
          </p>
        ) : null}
        <div className="hi-prev-actions">
          {action.kind === 'none' ? (
            <span className="hi-prev-note">{t('history.cantContinue')}</span>
          ) : (
            <button type="button" className="btn primary" onClick={() => act(row)}>
              {t(M.ACTION_KEY[action.kind])} <Kbd>⏎</Kbd>
            </button>
          )}
          <button type="button" className="btn" onClick={() => read(row)}>
            {t('history.read')} <Kbd>⌘↓</Kbd>
          </button>
          <button type="button" className="btn" onClick={() => openFolder(row)}>
            {t('history.openFolder')}
          </button>
        </div>
        {err ? (
          <p className="hi-prev-error" role="status">
            <span>{te(err)}</span>
            {err.detail ? <span className="mono hi-detail">{err.detail}</span> : null}
            {offerFork ? (
              <button type="button" className="btn" onClick={() => act(row, 'fork')}>
                {t('action.continueCopy')}
              </button>
            ) : null}
          </p>
        ) : null}
      </div>
    );
  };

  const popoverRow = popoverFor ? (rows.find((r) => r.uid === popoverFor) ?? null) : null;
  let flat = -1;

  return (
    <div className="hi" data-narrow={narrow ? 'true' : undefined}>
      <div className="hi-bar">
        <div className="hi-search">
          <SearchIcon size="sm" />
          <input
            ref={searchRef}
            type="search"
            className="hi-search-input"
            aria-label={t('history.search')}
            placeholder={t('history.placeholder')}
            value={text}
            maxLength={M.MAX_TEXT}
            spellCheck={false}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              if (e.code === 'ArrowDown' && !e.nativeEvent.isComposing && rows.length > 0) {
                e.preventDefault();
                roving.setActiveIndex(Math.max(0, index), true);
              }
            }}
          />
          <span className="hi-scope">{t('history.scopeLine', { scope: t('history.scope.titlesAndPrompts') })}</span>
        </div>
        <div className="hi-filters" role="group" aria-label={t('history.filters')}>
          <FilterSelect
            id="hi-f-project"
            label={t('history.filter.project')}
            value={projectId ?? ''}
            options={[{ value: '', label: all }, ...orderedProjects({ projects }).map((p) => ({ value: p.id, label: p.name }))]}
            onChange={(v) => setProjectId(v === '' ? null : (v as ProjectId))}
          />
          <FilterSelect
            id="hi-f-agent"
            label={t('history.filter.agent')}
            value={vendor ?? ''}
            options={[{ value: '', label: all }, ...VENDORS.map((v) => ({ value: v, label: vendorBadge(v) }))]}
            onChange={(v) => setVendor(v === '' ? null : (v as Vendor))}
          />
          <FilterSelect
            id="hi-f-when"
            label={t('history.filter.when')}
            value={when}
            options={M.WHENS.map((w) => ({ value: w, label: t(M.WHEN_KEY[w]) }))}
            onChange={(v) => setWhen(v as M.When)}
          />
          <FilterSelect
            id="hi-f-account"
            label={t('history.filter.account')}
            value={accountId ?? ''}
            options={[{ value: '', label: all }, ...[...accounts.values()].map((a) => ({ value: a.id, label: `${vendorBadge(a.vendor)} · ${a.label === 'Default' ? t('account.default') : a.label}` }))]}
            onChange={(v) => setAccountId(v === '' ? null : (v as AccountId))}
          />
          <FilterSelect
            id="hi-f-where"
            label={t('history.filter.where')}
            value={origin ?? ''}
            options={[{ value: '', label: all }, ...M.originChoices(orchestrator).map((o) => ({ value: o, label: t(M.ORIGIN_KEY[o]) }))]}
            onChange={(v) => setOrigin(v === '' ? null : (v as Origin))}
          />
          <span className="grow" />
          {coverage ? (
            <span className="hi-count">{t(next ? 'history.countMore' : 'history.count', { shown: rows.length, all: coverage.total })}</span>
          ) : null}
        </div>
        {indexing && coverage ? (
          <div className="hi-indexing" role="status">
            <span>{t('history.searching', { loaded: coverage.titlesLoaded, total: coverage.total })}</span>
            <div className="hi-progress" aria-hidden="true">
              <span style={{ width: `${coverage.total > 0 ? Math.min(100, (coverage.titlesLoaded / coverage.total) * 100) : 0}%` }} />
            </div>
          </div>
        ) : discovering ? (
          <div className="hi-indexing" role="status">
            <span>{t('firstRun.looking')}</span>
          </div>
        ) : null}
      </div>

      <div className="hi-main">
        <div className="hi-list-wrap">
          {error ? (
            <p className="hi-empty" role="status">
              {te(error)}
            </p>
          ) : loaded && rows.length === 0 && !indexing ? (
            <p className="hi-empty">{t(filtered ? 'history.none' : 'history.empty')}</p>
          ) : null}
          {rows.length > 0 ? (
            <div ref={listRef} role="listbox" aria-label={t('history.results')} className="hi-list">
              {groups.map((g, gi) => {
                const hid = `hi-g-${gi}`;
                return (
                  <div key={`${g.group}-${gi}`} role="group" aria-labelledby={hid} className="hi-grp">
                    <div id={hid} role="presentation" className="hi-grp-label">
                      {t(M.GROUP_KEY[g.group])}
                    </div>
                    {g.rows.map((row) => {
                      flat++;
                      const item = roving.getItemProps(flat);
                      const s = sessions.get(row.uid);
                      const live = row.running || (s !== undefined && s.control.kind !== 'notRunning');
                      return (
                        <div
                          key={row.uid}
                          id={`hi-row-${row.uid}`}
                          role="option"
                          aria-selected={row.uid === selected}
                          className="hi-row"
                          tabIndex={item.tabIndex}
                          data-roving-index={item['data-roving-index']}
                          ref={(el) => {
                            item.ref(el);
                            if (el) optionRefs.current.set(row.uid, el);
                            else optionRefs.current.delete(row.uid);
                          }}
                          onFocus={item.onFocus}
                          onKeyDown={(e) => onOptionKeyDown(e, row, item.onKeyDown)}
                          onClick={() => setSelected(row.uid)}
                          onDoubleClick={() => act(row)}
                        >
                          <StateGlyph state={live ? (s ? s.activity.state.kind : 'working') : 'ended'} />
                          <span className="hi-row-text">
                            <span className="hi-row-title">
                              {M.rowTitle(t, row, locale)}
                              {live ? <span className="hi-row-running"> ({t('history.running')})</span> : null}
                            </span>
                            <span className="hi-row-sub">{rowSub(row)}</span>
                          </span>
                        </div>
                      );
                    })}
                  </div>
                );
              })}
            </div>
          ) : null}
          {next ? (
            <div className="hi-more">
              <button type="button" className="btn ghost" onClick={loadMore}>
                {t('history.loadMore')}
              </button>
            </div>
          ) : null}
        </div>
        {!narrow ? (
          <section className="hi-preview" aria-label={t('history.preview')}>
            {selectedRow ? preview(selectedRow) : null}
          </section>
        ) : null}
      </div>

      <div className="hi-foot">
        <span>
          {t('history.retention')} {t('history.retentionMore')}
        </span>
        {truncated ? <span>{t('history.notIndexed')}</span> : null}
      </div>

      {narrow && popoverRow ? (
        <Popover
          open
          anchor={popoverAnchor}
          id="hi-preview-popover"
          label={t('history.preview')}
          className="hi-popover"
          onClose={() => setPopoverFor(null)}
        >
          {preview(popoverRow)}
        </Popover>
      ) : null}
    </div>
  );
}

function cap(map: Map<SessionUid, PreviewState>): Map<SessionUid, PreviewState> {
  while (map.size > PREVIEW_CACHE) {
    const first = map.keys().next().value as SessionUid | undefined;
    if (first === undefined) break;
    map.delete(first);
  }
  return map;
}
