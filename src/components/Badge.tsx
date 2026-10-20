/**
 * Text badge. Agents are distinguished by their name (`claude`, `codex`,
 * `cursor`, `agy`, `gemini`), never by a vendor logo (UI-GUIDELINES §2-3).
 */

import type { ReactNode } from 'react';

export interface BadgeProps {
  children: ReactNode;
  title?: string;
  /** `--warn` / `--error` / `--ok` text, used for `unverified`, `orphan`, … */
  tone?: 'neutral' | 'ok' | 'warn' | 'error';
}

export function Badge({ children, title, tone = 'neutral' }: BadgeProps) {
  return (
    <span className="badge" data-tone={tone} title={title}>
      {children}
    </span>
  );
}

export interface ChipProps {
  children: ReactNode;
  title?: string;
  onClick?: () => void;
  selected?: boolean;
}

/** Compact affordance chip: worktree, model, account. */
export function Chip({ children, title, onClick, selected }: ChipProps) {
  if (!onClick) {
    return (
      <span className="chip" data-selected={selected ? 'true' : undefined} title={title}>
        {children}
      </span>
    );
  }
  return (
    <button
      type="button"
      className="chip"
      data-selected={selected ? 'true' : undefined}
      title={title}
      onClick={onClick}
    >
      {children}
    </button>
  );
}

export interface KbdProps {
  children: ReactNode;
}

export function Kbd({ children }: KbdProps) {
  return <span className="kbd">{children}</span>;
}
