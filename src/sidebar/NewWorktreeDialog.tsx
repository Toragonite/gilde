/**
 * New worktree dialog (M1-PLAN T21 acceptance 5, DESIGN §7-1, §7-3, §7-6).
 *
 * The dialog's job is to make the consequence of pressing the button legible
 * *before* it is pressed: the slug the sanitizer will produce, the central path
 * the worktree will land in, and the branch git will be asked for. All three
 * are derived from the same rules `src-tauri/src/worktree/naming.rs` uses, so
 * what is shown is what is created — only the six random hex characters are
 * left as placeholders, because they do not exist until Rust makes them.
 *
 * `post_create_cmd` ships in M1 (DECISIONS §M1 세부 기본값) and remembers a
 * default per project.
 *
 * Cursor's alias convention is unverified on this machine (DESIGN §7-6, R11),
 * so it is labelled and starts unchecked instead of being quietly enabled.
 *
 * `NewWorktreeForm` is exported separately from the sheet because the sheet
 * renders through a DOM portal, which the static-render tests cannot mount.
 */

import { useEffect, useMemo, useState } from 'react';

import { Badge } from '../components/Badge';
import { Sheet } from '../components/Sheet';
import { useI18n } from '../i18n/useI18n';
import type {
  AgentBinary,
  AgentKindKey,
  AliasProviderKey,
  GraphNode,
  ProjectId,
} from '../lib/ipc';
import { useAppStore } from '../store/useAppStore';
import {
  ALIAS_PROVIDER_OPTIONS,
  previewBranch,
  previewSlug,
  previewWorktreePath,
  slugIssue,
} from '../store/worktreeSlice';
import type { SlugIssue } from '../store/worktreeSlice';
import './sidebar.css';

/** The pty is opened with a real winsize; codex renders nothing into 0×0. */
const INITIAL_COLS = 80;
const INITIAL_ROWS = 24;

function defaultProviders(): AliasProviderKey[] {
  return ALIAS_PROVIDER_OPTIONS.filter((o) => o.defaultOn).map((o) => o.key);
}

// ---------------------------------------------------------------------------
// Form
// ---------------------------------------------------------------------------

export interface NewWorktreeFormValue {
  projectId: ProjectId | '';
  taskName: string;
  baseRef: string;
  providers: AliasProviderKey[];
  postCreateCmd: string;
  launch: boolean;
  agent: AgentKindKey | '';
}

export interface NewWorktreeFormProps {
  value: NewWorktreeFormValue;
  projects: readonly { id: ProjectId; name: string }[];
  /** `repoSlug` of the chosen project, for the path preview. */
  repoSlug: string;
  defaultBaseRef: string;
  worktreesRoot: string;
  agents: readonly AgentBinary[];
  issue: SlugIssue | null;
  onChange: (patch: Partial<NewWorktreeFormValue>) => void;
  onSubmit: () => void;
}

export function NewWorktreeForm({
  value,
  projects,
  repoSlug,
  defaultBaseRef,
  worktreesRoot,
  agents,
  issue,
  onChange,
  onSubmit,
}: NewWorktreeFormProps) {
  const { t } = useI18n();
  const installed = agents.filter((binary) => binary.installed && binary.kind !== 'shell');

  const toggleProvider = (key: AliasProviderKey) =>
    onChange({
      providers: value.providers.includes(key)
        ? value.providers.filter((k) => k !== key)
        : [...value.providers, key],
    });

  return (
    <div className="sb-form">
      {projects.length === 0 ? (
        <span className="s12">{t('sidebar.projects.empty')}</span>
      ) : (
        <label className="sb-field">
          <span className="sb-label">{t('node.project')}</span>
          <select
            className="input"
            value={value.projectId}
            onChange={(event) => onChange({ projectId: event.target.value as ProjectId })}
          >
            {projects.map((project) => (
              <option key={project.id} value={project.id}>
                {project.name}
              </option>
            ))}
          </select>
        </label>
      )}

      <label className="sb-field">
        <span className="sb-label">{t('worktree.taskName')}</span>
        <input
          className="input"
          value={value.taskName}
          placeholder={t('worktree.taskNamePlaceholder')}
          onChange={(event) => onChange({ taskName: event.target.value })}
          onKeyDown={(event) => {
            if (event.key === 'Enter' && event.metaKey) {
              event.preventDefault();
              onSubmit();
            }
          }}
        />
      </label>

      {/* Live preview: what the sanitizer makes of the title, right now. */}
      <div className="sb-preview">
        <div className="sb-preview-row">
          <span className="sb-preview-key">{t('worktree.slug')}</span>
          <span className="sb-preview-val mono s12">{previewSlug(value.taskName)}</span>
        </div>
        <div className="sb-preview-row">
          <span className="sb-preview-key">{t('worktree.branch')}</span>
          <span className="sb-preview-val mono s12">{previewBranch(value.taskName)}</span>
        </div>
        <div className="sb-preview-row">
          <span className="sb-preview-key">{t('worktree.path')}</span>
          <span className="sb-preview-val mono s11">
            {previewWorktreePath(worktreesRoot, repoSlug, value.taskName)}
          </span>
        </div>
      </div>

      {issue ? (
        <span className="sb-issue">
          {issue === 'duplicate'
            ? t('worktree.slug.duplicate', { slug: previewSlug(value.taskName) })
            : issue === 'unsupported'
              ? t('worktree.slug.unsupported')
              : t('worktree.slug.empty')}
        </span>
      ) : null}

      <label className="sb-field">
        <span className="sb-label">{t('worktree.baseRef')}</span>
        <input
          className="input"
          value={value.baseRef}
          placeholder={defaultBaseRef || t('worktree.baseRefPlaceholder')}
          onChange={(event) => onChange({ baseRef: event.target.value })}
        />
      </label>

      <div className="sb-field">
        <span className="sb-label">{t('worktree.alias')}</span>
        {ALIAS_PROVIDER_OPTIONS.map((option) => (
          <label key={option.key} className="sb-check">
            <input
              type="checkbox"
              checked={value.providers.includes(option.key)}
              onChange={() => toggleProvider(option.key)}
            />
            <span className="mono s12">{t(`worktree.alias.${option.key}` as const)}</span>
            {option.verified ? null : <Badge tone="warn">{t('badge.unverified')}</Badge>}
          </label>
        ))}
        <span className="sb-hint">{t('worktree.alias.hint')}</span>
      </div>

      <label className="sb-field">
        <span className="sb-label">{t('worktree.postCreate')}</span>
        <input
          className="input mono"
          value={value.postCreateCmd}
          placeholder={t('worktree.postCreatePlaceholder')}
          onChange={(event) => onChange({ postCreateCmd: event.target.value })}
        />
        <span className="sb-hint">{t('worktree.postCreateHint')}</span>
      </label>

      <div className="sb-field">
        <label className="sb-check">
          <input
            type="checkbox"
            checked={value.launch}
            onChange={(event) => onChange({ launch: event.target.checked })}
          />
          <span>{t('worktree.launch')}</span>
        </label>
        {value.launch ? (
          installed.length === 0 ? (
            <span className="sb-hint">{t('session.new.noAgents')}</span>
          ) : (
            <div className="row gap-6">
              {installed.map((binary) => (
                <button
                  key={binary.kind}
                  type="button"
                  className="chip"
                  data-selected={binary.kind === value.agent ? 'true' : undefined}
                  title={binary.absPath ?? undefined}
                  onClick={() => onChange({ agent: binary.kind })}
                >
                  {binary.kind}
                </button>
              ))}
            </div>
          )
        ) : null}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Dialog
// ---------------------------------------------------------------------------

export function NewWorktreeDialog() {
  const { t } = useI18n();

  const open = useAppStore((s) => s.worktree.newWorktree.open);
  const requestedProjectId = useAppStore((s) => s.worktree.newWorktree.projectId);
  const roster = useAppStore((s) => s.worktree.roster);
  const postCreateDefaults = useAppStore((s) => s.worktree.postCreateCmd);
  const graph = useAppStore((s) => s.graph);
  const agents = useAppStore((s) => s.agents);
  const paths = useAppStore((s) => s.paths);
  const selectedNodeId = useAppStore((s) => s.ui.selectedNodeId);
  const setOpen = useAppStore((s) => s.setNewWorktreeOpen);
  const createWorktree = useAppStore((s) => s.createWorktree);
  const ensurePaths = useAppStore((s) => s.ensurePaths);

  const [value, setValue] = useState<NewWorktreeFormValue>({
    projectId: '',
    taskName: '',
    baseRef: '',
    providers: defaultProviders(),
    postCreateCmd: '',
    launch: false,
    agent: '',
  });
  const [busy, setBusy] = useState(false);

  /** The project of the selected node, so "new" from a row lands in the right one. */
  const projectFromSelection = useMemo(() => {
    let cursor = selectedNodeId;
    for (let hops = 0; cursor && hops < 8; hops++) {
      const node: GraphNode | undefined = graph.nodes.get(cursor);
      if (!node) break;
      if (node.kind === 'project') return node.id.slice('project:'.length);
      cursor = node.parentId;
    }
    return null;
  }, [selectedNodeId, graph]);

  useEffect(() => {
    if (!open) return;
    const resolved = requestedProjectId ?? projectFromSelection ?? roster[0]?.project.id ?? '';
    setValue({
      projectId: resolved,
      taskName: '',
      baseRef: '',
      providers: defaultProviders(),
      postCreateCmd: resolved ? (postCreateDefaults[resolved] ?? '') : '',
      launch: false,
      agent: '',
    });
    void ensurePaths();
    // Re-running on every store change would fight the user's own typing.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const entry = roster.find((row) => row.project.id === value.projectId);

  /** Slugs already taken in this project, read off the projection the tree renders. */
  const existingSlugs = useMemo(() => {
    if (!value.projectId) return [];
    const parent = `project:${value.projectId}`;
    const out: string[] = [];
    for (const node of graph.nodes.values()) {
      if (node.kind === 'worktree' && node.parentId === parent) out.push(node.label);
    }
    return out;
  }, [graph, value.projectId]);

  if (!open) return null;

  const issue = slugIssue(value.taskName, existingSlugs);
  const chosenAgent = agents.find((binary) => binary.kind === value.agent && binary.installed);
  const canSubmit =
    Boolean(value.projectId) && issue === null && !busy && (!value.launch || Boolean(chosenAgent));

  const close = () => setOpen(false);

  const submit = () => {
    if (!canSubmit || !value.projectId) return;
    setBusy(true);
    void createWorktree({
      projectId: value.projectId,
      taskName: value.taskName.trim(),
      aliasProviders: value.providers,
      ...(value.baseRef.trim() ? { baseRef: value.baseRef.trim() } : {}),
      postCreateCmd: value.postCreateCmd.trim(),
      ...(value.launch && value.agent
        ? { launch: { agent: value.agent, cols: INITIAL_COLS, rows: INITIAL_ROWS } }
        : {}),
    }).finally(() => setBusy(false));
  };

  const onChange = (patch: Partial<NewWorktreeFormValue>) => {
    setValue((current) => {
      const next = { ...current, ...patch };
      // Switching project brings that project's remembered post-create command.
      if (patch.projectId !== undefined && patch.projectId !== current.projectId) {
        next.postCreateCmd = postCreateDefaults[patch.projectId] ?? '';
      }
      return next;
    });
  };

  return (
    <Sheet
      open={open}
      title={t('worktree.new')}
      onClose={close}
      actions={
        <>
          {/* Cancel first: `Sheet` focuses it (UI-GUIDELINES §2-5). */}
          <button type="button" className="btn" onClick={close}>
            {t('common.cancel')}
          </button>
          <button type="button" className="btn primary" disabled={!canSubmit} onClick={submit}>
            {busy ? t('worktree.creating') : t('worktree.create')}
          </button>
        </>
      }
    >
      <NewWorktreeForm
        value={value}
        projects={roster.map((row) => ({ id: row.project.id, name: row.project.name }))}
        repoSlug={entry?.project.repoSlug ?? ''}
        defaultBaseRef={entry?.project.defaultBaseRef ?? ''}
        worktreesRoot={paths?.worktreesRoot ?? '~/.gilde/worktrees'}
        agents={agents}
        issue={issue}
        onChange={onChange}
        onSubmit={submit}
      />
    </Sheet>
  );
}
