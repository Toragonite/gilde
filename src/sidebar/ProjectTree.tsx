/**
 * The project tree (ux-ia §2.5, §7.5, §10): a flat ARIA `tree` (items carry
 * `aria-level`, `aria-posinset`, `aria-setsize`, `aria-expanded`), one Tab
 * stop with a roving tabindex.
 *
 *   ↑ ↓ Home End   move            → expand, or go to the first child
 *   ← collapse, or go to the parent ⏎ Space  open a session or team, fold the rest
 *   ⌥⇧↑ ⌥⇧↓  move a project (the order persists in `sidebarOrder`)
 *   ⌘D       add the focused session to the Split (other rows: nothing); a
 *            session row can also be dragged onto Main (T32, `splitAdd.ts`)
 *
 * Each item's accessible name carries every fact its row shows ("api-refactor,
 * needs approval for 2 min, checkout-service, claude, in Gilde"); ownership is
 * never a visual-only cue. A session that ends keeps its row while the
 * sidebar is hovered or focused (deferred reflow), then folds.
 */

import { useLayoutEffect, useRef } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent, MouseEvent as ReactMouseEvent } from 'react';

import { StateGlyph } from '../components/StateGlyph';
import { ChevronRightIcon, PlusIcon } from '../components/icons';
import { useEngagement } from '../hooks/useHold';
import { isImeComposing } from '../hooks/useHotkeys';
import { useRovingTabIndex } from '../hooks/useRovingTabIndex';
import { useI18n } from '../i18n/useI18n';
import { clock } from '../lib/clock';
import type { JsonValue } from '../lib/gen/serde_json/JsonValue';
import type { Session } from '../lib/gen/Session';
import type { SessionUid } from '../lib/gen/SessionUid';
import { vendorBadge } from '../lib/vendor';
import { appStore, useAppStore } from '../store/useAppStore';
import type { WorldState } from '../store/worldSlice';
import { itemAge } from '../shell/useAges';
import type { T } from '../shell/words';
import { ownerWord, sessionTitle, stateKey, stateWord, waitAge } from '../shell/words';
import type { Locale } from '../lib/gen/Locale';
import type { FlatItem, TreeNode } from './tree';
import { sessionDragProps, useSplitAddKey } from './splitAdd';
import { buildTree, flatten, isExpandable, isExpanded, moveProject, orderedProjects, parseFolds, parseOrder } from './tree';

/** The vendor word is left out for the default agent (ux-ia §2.5). */
const DEFAULT_VENDOR = 'claude';

function rowMeta(t: T, session: Session, isLead: boolean): string {
  const parts: string[] = [];
  if (isLead) parts.push(t('sidebar.lead'));
  if (session.vendor !== DEFAULT_VENDOR) parts.push(vendorBadge(session.vendor));
  const owner = ownerWord(t, session.control);
  if (owner) parts.push(owner);
  return parts.join(' · ');
}

/**
 * The facts a session row's accessible name adds to what it shows (ux-ia §10):
 * the state (with how long it has waited), the project, the vendor and the
 * ownership — so ownership is never a visual-only cue.
 */
function sessionFacts(t: T, world: WorldState, session: Session, locale: Locale): string {
  const parts: string[] = [];
  const word = stateWord(t, session);
  const item = world.queue.items.find((i) => i.subject.kind === 'session' && i.subject.uid === session.uid);
  parts.push(item && item.band <= 2 ? t('sidebar.waitingFor', { state: word, age: waitAge(t, itemAge(item, world.queue.rev), locale) }) : word);
  const project = session.projectId ? world.projects.get(session.projectId)?.name : null;
  if (project) parts.push(project);
  parts.push(vendorBadge(session.vendor));
  if (session.control.kind === 'watching') parts.push(t('ownership.runsIn', { owner: ownerWord(t, session.control) ?? '' }));
  else if (session.control.kind === 'inGilde') parts.push(t('ownership.inGilde'));
  return parts.join(', ');
}

function toggleFold(key: string, open: boolean): void {
  const s = appStore.getState();
  const folds = parseFolds(s.ui.sidebarFolds);
  s.setSidebarFolds({ ...folds, [key]: open } as unknown as JsonValue);
}

export function ProjectTree() {
  const { t, locale } = useI18n();
  const world = useAppStore((s) => s.world);
  const order = useAppStore((s) => s.ui.sidebarOrder);
  const folds = useAppStore((s) => s.ui.sidebarFolds);
  const view = useAppStore((s) => s.ui.view);
  const engagement = useEngagement();
  const treeRef = useRef<HTMLUListElement>(null);
  useSplitAddKey(treeRef);

  // Deferred reflow: rows shown while the sidebar is engaged stay until it is not.
  const shownRef = useRef<Set<SessionUid>>(new Set());
  const stickyRef = useRef<Set<SessionUid>>(new Set());
  if (engagement.engaged) for (const uid of shownRef.current) stickyRef.current.add(uid);
  else stickyRef.current = new Set();

  const queued = new Set<SessionUid>();
  for (const item of world.queue.items) if (item.subject.kind === 'session') queued.add(item.subject.uid);
  const tree = buildTree(world, { order, folds, now: clock.wall(), sticky: stickyRef.current, queued });
  const flat = flatten(tree);
  const roving = useRovingTabIndex(flat.length);

  useLayoutEffect(() => {
    const shown = new Set<SessionUid>();
    for (const f of flat) if (f.node.kind === 'session') shown.add(f.node.session.uid);
    shownRef.current = shown;
  });

  const selectedKey =
    view.screen === 'session' ? `s:${view.uid}` : view.screen === 'team' ? `t:${view.teamKey}` : null;

  const activate = (node: TreeNode) => {
    const s = appStore.getState();
    switch (node.kind) {
      case 'session':
        s.openSession(node.session.uid);
        return;
      case 'team':
        s.navigate({ screen: 'team', teamKey: node.team.key, mode: s.ui.teamLayouts[node.team.key]?.mode ?? 'panes' });
        return;
      case 'member':
        return;
      default:
        if (isExpandable(node)) toggleFold(node.key, !isExpanded(node));
    }
  };

  const onKeyDown = (event: ReactKeyboardEvent<HTMLElement>, index: number, item: FlatItem) => {
    const { node } = item;
    if (isImeComposing(event.nativeEvent) || event.ctrlKey || event.metaKey) return;
    if (event.altKey && event.shiftKey && (event.code === 'ArrowUp' || event.code === 'ArrowDown') && node.kind === 'project') {
      event.preventDefault();
      if (event.repeat) return;
      const s = appStore.getState();
      const live = orderedProjects(s.world.projects.values(), parseOrder(s.ui.sidebarOrder)).filter((p) => !p.earlier);
      s.setSidebarOrder(moveProject(live, node.project.id, event.code === 'ArrowUp' ? -1 : 1) as unknown as JsonValue);
      return;
    }
    if (event.altKey) return;
    switch (event.code) {
      case 'ArrowRight':
        event.preventDefault();
        if (isExpandable(node) && !isExpanded(node)) toggleFold(node.key, true);
        else if (isExpanded(node)) roving.setActiveIndex(index + 1, true);
        return;
      case 'ArrowLeft': {
        event.preventDefault();
        if (isExpanded(node)) {
          toggleFold(node.key, false);
          return;
        }
        const parent = flat.findIndex((f) => f.node.key === item.parent);
        if (parent >= 0) roving.setActiveIndex(parent, true);
        return;
      }
      case 'Enter':
      case 'Space':
        event.preventDefault();
        if (!event.repeat) activate(node);
        return;
      default:
        roving.getItemProps(index).onKeyDown(event);
    }
  };

  const onClick = (event: ReactMouseEvent, index: number, node: TreeNode) => {
    event.preventDefault();
    roving.setActiveIndex(index);
    activate(node);
  };

  return (
    <section className="sb-tree-sec" aria-labelledby="sb-tree-h">
      <div className="sb-sec">
        <h2 id="sb-tree-h" className="sb-sec-title">
          {t('shell.projects')}
        </h2>
      </div>
      <ul ref={treeRef} className="sb-tree" role="tree" aria-labelledby="sb-tree-h" {...engagement.props}>
        {flat.map((item, index) => {
          const { node } = item;
          const props = roving.getItemProps(index);
          const expandable = isExpandable(node);
          const common = {
            role: 'treeitem' as const,
            'aria-level': node.level,
            'aria-posinset': item.pos,
            'aria-setsize': item.size,
            'aria-expanded': expandable ? isExpanded(node) : undefined,
            'aria-selected': node.key === selectedKey,
            tabIndex: props.tabIndex,
            ref: props.ref,
            onFocus: props.onFocus,
            'data-roving-index': props['data-roving-index'],
            onKeyDown: (event: ReactKeyboardEvent<HTMLElement>) => onKeyDown(event, index, item),
            onClick: (event: ReactMouseEvent) => onClick(event, index, node),
            className: `sb-row sb-row-${node.kind}`,
            style: { ['--lvl' as string]: node.level - 1 },
          };
          const chevron = expandable ? (
            <ChevronRightIcon size="sm" className="sb-chev" data-open={isExpanded(node) ? 'true' : undefined} />
          ) : (
            <span className="sb-chev-sp" aria-hidden="true" />
          );

          switch (node.kind) {
            case 'project':
              return (
                <li key={node.key} {...common}>
                  {chevron}
                  <span className="sb-title ell">{node.project.name}</span>
                  <span className="sb-count mono" aria-hidden="true">
                    {node.project.liveSessions > 0 ? node.project.liveSessions : ''}
                  </span>
                  <span className="g-sr-only">, {t('shell.running', { n: node.project.liveSessions })}</span>
                  <button
                    type="button"
                    className="btn ghost icon sb-add"
                    tabIndex={-1}
                    aria-label={t('sidebar.newIn', { project: node.project.name })}
                    onClick={(event) => {
                      event.stopPropagation();
                      appStore.getState().openSheet('newSession', { projectId: node.project.id });
                    }}
                  >
                    <PlusIcon size="sm" />
                  </button>
                </li>
              );
            case 'earlier':
              return (
                <li key={node.key} {...common}>
                  {chevron}
                  <span className="sb-title ell">{t('nav.earlierProjects')}</span>
                  <span className="sb-count mono">{node.children.length}</span>
                  <span className="g-sr-only">, {t('sidebar.earlierNote')}</span>
                </li>
              );
            case 'team': {
              const name = t('sidebar.team', { name: node.team.name });
              const word = t(stateKey(node.team.glyphState));
              return (
                <li key={node.key} {...common}>
                  {chevron}
                  <StateGlyph state={node.team.glyphState.kind} />
                  <span className="sb-title ell">{name}</span>
                  <span className="sb-count mono" aria-hidden="true">
                    {node.memberCount}
                  </span>
                  <span className="g-sr-only">
                    , {word}, {t('sidebar.teamMembers', { n: node.memberCount })}
                  </span>
                </li>
              );
            }
            case 'ended':
              return (
                <li key={node.key} {...common}>
                  {chevron}
                  <StateGlyph state="ended" />
                  <span className="sb-title ell sb-dim">{t('sidebar.endedToday', { n: node.children.length })}</span>
                </li>
              );
            case 'member':
              return (
                <li key={node.key} {...common}>
                  {chevron}
                  <StateGlyph state="statusUnknown" />
                  {node.member.colour ? <span className="swatch" data-colour={node.member.colour} aria-hidden="true" /> : null}
                  <span className="sb-title ell">{node.member.name}</span>
                  {node.member.role === 'lead' ? <span className="sb-meta">{t('sidebar.lead')}</span> : null}
                </li>
              );
            case 'session': {
              const session = node.session;
              const isLead = node.member?.role === 'lead';
              const title = node.member ? node.member.name : sessionTitle(t, session, locale);
              const ended = session.control.kind === 'notRunning';
              const meta = rowMeta(t, session, isLead);
              return (
                <li key={node.key} {...common} {...sessionDragProps(session.uid)} data-ended={ended ? 'true' : undefined}>
                  {chevron}
                  <StateGlyph state={session.activity.state.kind} likely={session.activity.confidence === 'likely'} />
                  {node.member?.colour ? <span className="swatch" data-colour={node.member.colour} aria-hidden="true" /> : null}
                  <span className="sb-title ell">{title}</span>
                  {meta ? <span className="sb-meta ell">{meta}</span> : null}
                  <span className="g-sr-only">, {sessionFacts(t, world, session, locale)}</span>
                </li>
              );
            }
          }
        })}
      </ul>
    </section>
  );
}
