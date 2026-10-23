/**
 * "Close pane…" (contract §7.4.7, A21.4; ux-ia §3.8, row 105): the sheet that
 * closes a hosted teammate's pane. Prop-less (A23.5): it renders while
 * `ui.sheet === 'closePane'` and acts on the Team screen's focused member
 * (`teamLayouts[teamKey].focused`, set by whoever opened it), then closes
 * with `closeSheet()`.
 *
 * The preview's sentences arrive as `lines: Notice[]` — `team.closePane`, then
 * `team.closePaneUnread {member}` when unread messages are possible — and each
 * is rendered through i18n (a param never holds a key name). A destructive
 * sheet: focus starts on Cancel and `⌘⏎` never confirms it. The lead is not
 * told; `⌘W` on a tile closes only the view.
 */

import { useEffect, useRef, useState } from 'react';

import { Sheet } from '../components/Sheet';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { TeamKey } from '../lib/gen/TeamKey';
import type { TeamMemberClosePreviewRes } from '../lib/gen/TeamMemberClosePreviewRes';
import type { IpcError } from '../lib/ipcError';
import { useAppStore } from '../store/useAppStore';

interface Target {
  teamKey: TeamKey;
  memberKey: MemberKey;
}

export function ClosePaneSheet() {
  const open = useAppStore((s) => s.ui.sheet === 'closePane');
  const teamKey = useAppStore((s) => (s.ui.view.screen === 'team' ? s.ui.view.teamKey : null));
  const memberKey = useAppStore((s) => (teamKey ? (s.ui.teamLayouts[teamKey]?.focused ?? null) : null));
  const closeSheet = useAppStore((s) => s.closeSheet);
  const target: Target | null = teamKey && memberKey ? { teamKey, memberKey } : null;

  // Opened without a member to act on: nothing to show, so close again.
  const hasTarget = target !== null;
  useEffect(() => {
    if (open && !hasTarget) closeSheet();
  }, [open, hasTarget, closeSheet]);

  if (!open || !target) return null;
  return <CloseBody key={`${target.teamKey}:${target.memberKey}`} target={target} close={closeSheet} />;
}

function CloseBody({ target, close }: { target: Target; close: () => void }) {
  const { t, td, te } = useI18n();
  const [preview, setPreview] = useState<TeamMemberClosePreviewRes | null>(null);
  const [error, setError] = useState<IpcError | null>(null);
  const [busy, setBusy] = useState(false);
  const cancelRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    let live = true;
    commands.team_member_close_preview({ teamKey: target.teamKey, memberKey: target.memberKey }).then(
      (res) => live && setPreview(res),
      (e: IpcError) => live && setError(e),
    );
    return () => {
      live = false;
    };
  }, [target.teamKey, target.memberKey]);

  const confirm = () => {
    if (!preview || busy) return;
    setBusy(true);
    commands.team_member_close({ teamKey: target.teamKey, memberKey: target.memberKey, confirmToken: preview.confirmToken }).then(
      () => close(),
      (e: IpcError) => {
        setBusy(false);
        setError(e);
      },
    );
  };

  return (
    <Sheet
      open
      destructive
      title={t('team.closePaneTitle')}
      onCancel={close}
      initialFocus={cancelRef}
      actions={
        <>
          <button ref={cancelRef} type="button" className="g-sbtn" onClick={close}>
            {t('common.cancel')}
          </button>
          <button type="button" className="g-sbtn g-sbtn-danger" disabled={!preview || busy} onClick={confirm}>
            {t('team.closePaneTitle')}
          </button>
        </>
      }
    >
      {preview ? (
        preview.lines.map((line, i) => (
          <p key={`${line.key}-${i}`} className="g-close-line">
            {td(line.key, line.params)}
          </p>
        ))
      ) : !error ? (
        <p className="g-close-line">{t('common.loading')}</p>
      ) : null}
      {error && (
        <p className="g-close-line" role="status">
          {te(error)}
          {error.detail ? <span className="mono"> {error.detail}</span> : null}
        </p>
      )}
    </Sheet>
  );
}
