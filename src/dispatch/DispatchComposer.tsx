/**
 * Dispatch composer — ⌘⇧D (DESIGN §9-7, acceptance 1–3).
 *
 * One prompt, N agents, one worktree strategy. Everything the sheet collects
 * maps one-to-one onto `DispatchSpec`, so there is no hidden default between
 * what the user sees and what `dispatch_create` receives.
 *
 * The two things this form refuses to do:
 *
 *  - **It never waits for quota.** The chips inside `AgentPicker` load in the
 *    background and are advisory; the sheet is usable the moment it opens and
 *    the submit button does not consult them (acceptance 2).
 *  - **It never routes silently.** `Auto (route)` is an explicit target, and
 *    the decision is shown before anything runs (DECISIONS: no built-in
 *    orchestration philosophy).
 *
 * `taskSlug` is validated with the same `[a-z0-9]` pipeline
 * `src-tauri/src/worktree/naming.rs::sanitize` runs, because
 * `dispatch::engine::create` refuses `emptySlug` on exactly that result — the
 * preview under the field is what Rust will actually create.
 *
 * **Layout (M1.5-A-dispatch, BACKLOG 10).** The form is a flex column: the
 * fields live in `.dsp-composer-body`, which is the only thing that scrolls,
 * and the action row is that scroller's *sibling*. Inside the sheet the row is
 * hoisted one level further, into `Sheet`'s own `actions` slot, so 취소/실행
 * are outside `.sheet-body` and stay on screen in a 900 px window instead of
 * being pushed below the fold.
 */

import { useEffect, useMemo, useRef } from 'react';

import { Kbd } from '../components/Badge';
import { Mono } from '../components/Mono';
import { Sheet } from '../components/Sheet';
import { useI18n } from '../i18n/useI18n';
import type { MessageKey } from '../i18n';
import type { PermissionMode, ProjectId, WorktreeId } from '../lib/ipc';
import { useAppStore } from '../store/useAppStore';
import {
  AUTO_VENDOR,
  PERMISSION_MODES,
  sanitizeSlug,
  slugIssue,
  titleOfPrompt,
} from '../store/dispatchSlice';
import { AgentPicker } from './AgentPicker';
import './dispatch.css';

const STRATEGIES = ['perTarget', 'shared', 'existing'] as const;

/**
 * The one-line explanation under the permission select (UI-GUIDELINES §2-4).
 *
 * A mode this build does not model gets no sentence rather than a guessed one;
 * `PermissionMode` is an open string type because the vendor, not this app,
 * owns the vocabulary.
 */
function permissionHintKey(mode: PermissionMode): MessageKey | null {
  return PERMISSION_MODES.includes(mode)
    ? (`dispatch.composer.permissionHint.${mode}` as MessageKey)
    : null;
}

/**
 * Everything the action row needs, so it can render inside the form *or* in
 * the sheet footer without either copy owning the state.
 */
function useComposerActions() {
  const { t } = useI18n();
  const composer = useAppStore((s) => s.dispatch.composer);
  const setComposerOpen = useAppStore((s) => s.setComposerOpen);
  const submitComposer = useAppStore((s) => s.submitComposer);
  const openTab = useAppStore((s) => s.openTab);

  const selectedCount = useMemo(
    () => Object.values(composer.targets).filter((draft) => draft.selected).length,
    [composer.targets],
  );

  const canSubmit =
    !composer.busy &&
    composer.prompt.trim().length > 0 &&
    Boolean(composer.projectId) &&
    selectedCount > 0 &&
    slugIssue(composer.taskSlug) === null &&
    (composer.strategy !== 'existing' || Boolean(composer.existingWorktreeId));

  const close = () => setComposerOpen(false);

  const submit = async () => {
    if (!canSubmit) return;
    const task = await submitComposer();
    if (!task) return;
    openTab({
      title: t('dispatch.title'),
      sublabel: task.title,
      descriptor: { view: 'dispatch', taskId: task.id },
      closable: true,
    });
    close();
  };

  return { busy: composer.busy, canSubmit, selectedCount, close, submit };
}

/**
 * Cancel · ⌘↵ · Run. A fixed footer: it is never inside the element that
 * scrolls (BACKLOG 10).
 */
export function DispatchComposerActions() {
  const { t } = useI18n();
  const { busy, canSubmit, selectedCount, close, submit } = useComposerActions();

  return (
    <div className="row gap-6 dsp-composer-actions">
      <div className="grow" />
      <button type="button" className="btn" onClick={close}>
        {t('common.cancel')}
      </button>
      <Kbd>⌘↵</Kbd>
      <button
        type="button"
        className="btn primary"
        disabled={!canSubmit}
        onClick={() => void submit()}
      >
        {busy
          ? t('dispatch.composer.submitting')
          : t('dispatch.composer.submit', { n: selectedCount })}
      </button>
    </div>
  );
}

export interface DispatchComposerFormProps {
  /**
   * Render the action row at the bottom of the form. The sheet turns this off
   * and renders `DispatchComposerActions` in its own footer instead, which is
   * what keeps the buttons out of the scrolling body.
   */
  inlineActions?: boolean;
}

/**
 * The composer without its sheet chrome.
 *
 * Split out so the form can be rendered — and asserted on — without a DOM:
 * `Sheet` portals into `document.body`, which does not exist under the
 * server-rendered unit tests.
 */
export function DispatchComposerForm({ inlineActions = true }: DispatchComposerFormProps = {}) {
  const { t, td } = useI18n();

  const composer = useAppStore((s) => s.dispatch.composer);
  const roster = useAppStore((s) => s.worktree.roster);
  const loadRoster = useAppStore((s) => s.loadRoster);
  const patchComposer = useAppStore((s) => s.patchComposer);
  const { selectedCount, submit } = useComposerActions();

  const promptRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (!composer.open) return;
    void loadRoster();
    // `Sheet` focuses the first action button on mount; a composer wants the
    // prompt, so this runs after that.
    const timer = setTimeout(() => promptRef.current?.focus(), 0);
    return () => clearTimeout(timer);
  }, [composer.open, loadRoster]);

  // Default the project to the only one there is, or the first.
  useEffect(() => {
    if (!composer.open || composer.projectId) return;
    const first = roster[0]?.project;
    if (first) patchComposer({ projectId: first.id, baseRef: '' });
  }, [composer.open, composer.projectId, roster, patchComposer]);

  const project = useMemo(
    () => roster.find((entry) => entry.project.id === composer.projectId),
    [roster, composer.projectId],
  );

  const issue = slugIssue(composer.taskSlug);
  const slugPreview = sanitizeSlug(composer.taskSlug);
  const permissionHint = permissionHintKey(composer.permissionMode);

  return (
    <div className="col gap-12 dsp-composer">
      <div className="col gap-12 dsp-composer-body">
        <label className="col gap-4">
          <span className="muted s11">{t('dispatch.composer.prompt')}</span>
          <textarea
            ref={promptRef}
            className="textarea dsp-prompt"
            value={composer.prompt}
            placeholder={t('dispatch.composer.promptPlaceholder')}
            onChange={(event) => patchComposer({ prompt: event.target.value })}
            onKeyDown={(event) => {
              if (event.key === 'Enter' && event.metaKey) {
                event.preventDefault();
                void submit();
              }
            }}
          />
        </label>

        <div className="dsp-fields">
          <label className="col gap-4">
            <span className="muted s11">{t('dispatch.composer.project')}</span>
            {roster.length === 0 ? (
              <span className="s12">{t('dispatch.composer.noProject')}</span>
            ) : (
              <select
                className="input"
                value={composer.projectId}
                onChange={(event) =>
                  patchComposer({
                    projectId: event.target.value as ProjectId,
                    existingWorktreeId: '',
                  })
                }
              >
                {roster.map((entry) => (
                  <option key={entry.project.id} value={entry.project.id}>
                    {entry.project.name}
                  </option>
                ))}
              </select>
            )}
          </label>

          <label className="col gap-4">
            <span className="muted s11">{t('dispatch.composer.baseRef')}</span>
            <input
              className="input"
              value={composer.baseRef}
              placeholder={
                project?.project.defaultBaseRef ?? t('dispatch.composer.baseRefPlaceholder')
              }
              onChange={(event) => patchComposer({ baseRef: event.target.value })}
            />
          </label>

          <label className="col gap-4">
            <span className="muted s11">{t('dispatch.composer.slug')}</span>
            <input
              className="input"
              value={composer.taskSlug}
              onChange={(event) => patchComposer({ taskSlug: event.target.value })}
            />
          </label>

          <label className="col gap-4">
            <span className="muted s11">{t('dispatch.composer.strategy')}</span>
            <select
              className="input"
              value={composer.strategy}
              onChange={(event) =>
                patchComposer({ strategy: event.target.value as (typeof STRATEGIES)[number] })
              }
            >
              {STRATEGIES.map((strategy) => (
                <option key={strategy} value={strategy}>
                  {t(`dispatch.composer.strategy.${strategy}` as MessageKey)}
                </option>
              ))}
            </select>
          </label>

          {composer.strategy === 'existing' ? (
            <label className="col gap-4">
              <span className="muted s11">{t('dispatch.composer.existingWorktree')}</span>
              <select
                className="input"
                value={composer.existingWorktreeId}
                onChange={(event) =>
                  patchComposer({ existingWorktreeId: event.target.value as WorktreeId })
                }
              >
                <option value="">{t('common.none')}</option>
                {(project?.worktrees ?? []).map((worktree) => (
                  <option key={worktree.id} value={worktree.id}>
                    {worktree.slug} · {worktree.branch}
                  </option>
                ))}
              </select>
            </label>
          ) : null}

          <label className="col gap-4">
            <span className="muted s11">{t('dispatch.composer.timeout')}</span>
            <input
              className="input"
              type="number"
              min={1}
              value={composer.timeoutMinutes}
              onChange={(event) =>
                patchComposer({ timeoutMinutes: Math.max(1, Number(event.target.value) || 1) })
              }
            />
          </label>

          {/* The mode is claude's word for the whole task; Rust translates it
              per vendor (`dispatch::engine::permission_mode_for`), so the
              option carries the wire token and the line under it says what the
              run may touch. `default` is kept as an explicit choice — it is
              simply not the one a headless run can work under. */}
          <label className="col gap-4 dsp-permission">
            <span className="muted s11">{t('dispatch.composer.permission')}</span>
            <select
              className="input"
              value={composer.permissionMode}
              onChange={(event) => patchComposer({ permissionMode: event.target.value })}
            >
              {PERMISSION_MODES.map((mode) => (
                <option key={mode} value={mode}>
                  {t(`dispatch.composer.permission.${mode}` as MessageKey)}
                </option>
              ))}
            </select>
            {permissionHint ? <span className="s11 muted">{t(permissionHint)}</span> : null}
          </label>
        </div>

        <div className="row gap-8">
          <Mono small className="muted ell">
            {issue === null
              ? t('dispatch.composer.slugPreview', { slug: slugPreview })
              : t(`dispatch.composer.slug.${issue}` as MessageKey)}
          </Mono>
          <div className="grow" />
          {composer.prompt.trim() ? (
            <span className="s11 muted ell">{titleOfPrompt(composer.prompt)}</span>
          ) : null}
        </div>

        <AgentPicker prompt={composer.prompt} />

        {selectedCount === 0 ? (
          <span className="s11 muted">{t('dispatch.composer.noTargets')}</span>
        ) : null}

        {composer.error ? (
          <span className="s12 dsp-error">{td(composer.error.key, composer.error.params)}</span>
        ) : null}

        {composer.targets[AUTO_VENDOR]?.selected ? (
          <span className="s11 muted">{t('dispatch.picker.autoHint')}</span>
        ) : null}
      </div>

      {inlineActions ? <DispatchComposerActions /> : null}
    </div>
  );
}

export function DispatchComposer() {
  const { t } = useI18n();
  const open = useAppStore((s) => s.dispatch.composer.open);
  const setComposerOpen = useAppStore((s) => s.setComposerOpen);

  if (!open) return null;

  return (
    <Sheet
      open={open}
      title={t('dispatch.composer.title')}
      onClose={() => setComposerOpen(false)}
      // The footer belongs to the sheet, not to the form: `.sheet-actions` is
      // a sibling of the scrolling `.sheet-body`, so the buttons survive a
      // window short enough to make the form overflow (BACKLOG 10).
      actions={<DispatchComposerActions />}
    >
      <DispatchComposerForm inlineActions={false} />
    </Sheet>
  );
}
