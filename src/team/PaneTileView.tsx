/**
 * A non-member pane of a hosted team (contract A30.1, §7.4.5): what PaneHost's
 * pane table shows that is not a teammate's session.
 *
 *  - `Starting` reads "starting…" and turns into `team.didNotStart` when the
 *    spawn failed or 30 s after `since` — by the UI's own clock, there is no
 *    core timer — with [Close tile].
 *  - `Unknown` reads `team.unknownPane {lead}`: never a member, never in the
 *    queue, never focused or zoomed for the user; a running one shows its
 *    terminal read-only (`pty_attach`, inert: no keys reach it); an exited and
 *    kept one shows "Crashed · exit {code}" with [Close tile].
 *
 * [Close tile] closes only the view (the pane is the lead's; ⌘W semantics).
 */

import { useEffect, useState } from 'react';
import type { CSSProperties } from 'react';

import { StateGlyph } from '../components/StateGlyph';
import type { GlyphKind } from '../components/StateGlyph';
import { useI18n } from '../i18n/useI18n';
import { clock } from '../lib/clock';
import type { PaneTile } from '../lib/gen/PaneTile';
import { TerminalView } from '../terminal/TerminalView';
import { CAP, flat } from './model';

/** A placeholder with no `respawn-pane` this long after it appeared did not start (§7.4.5). */
export const DID_NOT_START_MS = 30_000;

export type PaneTilePhase = 'starting' | 'didNotStart' | 'unknownRunning' | 'unknownExited';

/** What a pane tile shows at wall-clock time `now`. */
export function paneTilePhase(tile: PaneTile, now: number): PaneTilePhase {
  const st = tile.state;
  if (st.kind === 'starting') return st.spawnFailed || now >= st.since + DID_NOT_START_MS ? 'didNotStart' : 'starting';
  return st.running ? 'unknownRunning' : 'unknownExited';
}

/** The key a pane tile is known by in tile lists and the strip. */
export function paneKey(tile: PaneTile): string {
  return `pane:${tile.paneId}`;
}

export function paneGlyph(phase: PaneTilePhase): GlyphKind {
  switch (phase) {
    case 'starting':
      return 'starting';
    case 'didNotStart':
      return 'error';
    case 'unknownExited':
      return 'crashed';
    case 'unknownRunning':
      return 'statusUnknown';
  }
}

/** Re-render once when a `Starting` tile crosses its 30 s mark. */
export function usePaneTilePhase(tile: PaneTile): PaneTilePhase {
  const [, tick] = useState(0);
  const phase = paneTilePhase(tile, clock.wall());
  const since = tile.state.kind === 'starting' ? tile.state.since : null;
  useEffect(() => {
    if (phase !== 'starting' || since === null) return;
    const delay = Math.max(0, since + DID_NOT_START_MS - clock.wall()) + 5;
    const timer = setTimeout(() => tick((n) => n + 1), delay);
    return () => clearTimeout(timer);
  }, [phase, since]);
  return phase;
}

export interface PaneTileViewProps {
  tile: PaneTile;
  /** The lead's display name, for "Unknown pane started by {lead}". */
  leadName: string;
  /** Position among the screen's tiles (for the accessible name). */
  index: number;
  count: number;
  style?: CSSProperties;
  isNew?: boolean;
  onClose: () => void;
}

export function PaneTileView({ tile, leadName, index, count, style, isNew = false, onClose }: PaneTileViewProps) {
  const { t } = useI18n();
  const phase = usePaneTilePhase(tile);
  const unknownLine = t('team.unknownPane', { lead: leadName });
  const title = tile.title ? flat(tile.title, CAP.name) : null;
  const name = title ?? (phase === 'unknownRunning' || phase === 'unknownExited' ? unknownLine : t('team.placeholder'));
  const exitCode = tile.state.kind === 'unknown' ? tile.state.exitCode : null;
  const line =
    phase === 'starting'
      ? t('tile.starting')
      : phase === 'didNotStart'
        ? t('team.didNotStart')
        : phase === 'unknownExited'
          ? t('tile.crashed', { code: exitCode ?? t('common.unknown') })
          : title
            ? unknownLine
            : null;
  const closable = phase === 'didNotStart' || phase === 'unknownExited';
  const stateWord = phase === 'starting' ? t('tile.starting') : phase === 'didNotStart' ? t('team.didNotStart') : unknownLine;

  return (
    <section
      role="region"
      aria-label={t('tile.label', { index: index + 1, tiles: count, name, state: stateWord })}
      className="g-tile g-pane-tile"
      data-pane={tile.paneId}
      data-phase={phase}
      data-new={isNew ? 'true' : undefined}
      style={style}
    >
      <div className="g-tile-head">
        <StateGlyph state={paneGlyph(phase)} />
        <span className="g-tile-name">{name}</span>
      </div>
      {title && (phase === 'unknownRunning' || phase === 'unknownExited') && <p className="g-tile-line">{unknownLine}</p>}
      {closable && (
        <p className="g-tile-line g-tile-notice">
          <span>{line}</span>
          <button type="button" className="g-sbtn" onClick={onClose}>
            {t('action.closeTile')}
          </button>
        </p>
      )}
      <div className="g-tile-body">
        {phase === 'unknownRunning' && tile.pty ? (
          // Read-only: inert keeps keys and focus out; the lead owns this pane.
          <div className="g-pane-term" inert>
            <TerminalView ptyId={tile.pty} label={name} active={false} />
          </div>
        ) : phase === 'starting' ? (
          <p className="g-tile-empty">{line}</p>
        ) : null}
      </div>
    </section>
  );
}
