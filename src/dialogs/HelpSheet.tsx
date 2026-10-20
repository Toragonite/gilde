/**
 * ⌘/ — the keyboard reference (T27, DESIGN §9-8).
 *
 * It renders `helpEntries()`, which is `HELP_ENTRIES` filtered to the bindings
 * the current build claims (`activeHotkeyIds`), so the sheet cannot document a
 * binding that does not exist or miss one that does — `palette.test.tsx`
 * asserts the two sets match exactly, in the preview and with the lanes on.
 *
 * The closing note is the part users actually need: a terminal pane swallows
 * every key except the ⌘ combos listed here, which is why ⌘B works over a live
 * agent TUI while Ctrl+C still reaches the process.
 */

import { Kbd } from '../components/Badge';
import { experimentalLanes } from '../features';
import { Sheet } from '../components/Sheet';
import { useI18n } from '../i18n/useI18n';
import { HELP_GROUPS, HELP_GROUP_LABEL, formatCombo, helpEntries } from '../palette/commands';
import { useAppStore } from '../store/useAppStore';
import '../palette/palette.css';

export function HelpSheet() {
  const { t } = useI18n();
  const open = useAppStore((s) => s.ui.helpOpen);
  const setHelpOpen = useAppStore((s) => s.setHelpOpen);

  if (!open) return null;

  const close = () => setHelpOpen(false);
  const entries = helpEntries();
  const lanes = experimentalLanes();

  return (
    <Sheet
      open={open}
      title={t('help.title')}
      onClose={close}
      actions={
        <button type="button" className="btn" onClick={close}>
          {t('common.close')}
        </button>
      }
    >
      <div className="help-groups">
        {HELP_GROUPS.map((group) => {
          const rows = entries.filter((entry) => entry.group === group);
          if (rows.length === 0) return null;
          return (
            <div className="help-group" key={group}>
              <div className="help-group-title">{t(HELP_GROUP_LABEL[group])}</div>
              {rows.map((entry) => (
                <div className="help-row" key={entry.id}>
                  <span className="grow ell">{t(entry.labelKey)}</span>
                  <Kbd>{entry.display ?? formatCombo(entry.combo)}</Kbd>
                </div>
              ))}
            </div>
          );
        })}
      </div>
      {lanes ? <p className="help-note">{t('help.terminalNote')}</p> : null}
    </Sheet>
  );
}
