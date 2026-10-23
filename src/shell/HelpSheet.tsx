/**
 * The keyboard help sheet (`⌘/`), generated from T13's `KEYMAP` — so it is
 * locked to the bindings (ux-ia §7.1 Help): a binding that changes changes
 * the sheet. Labels are `keys.<id>`; a KEYMAP row whose label is not in the
 * tables shows its id rather than disappearing.
 */

import { Sheet } from '../components/Sheet';
import { KEYMAP, formatCombo } from '../hooks/useHotkeys';
import type { HotkeyScope, KeymapEntry } from '../hooks/useHotkeys';
import { isMessageKey } from '../i18n';
import type { MessageKey } from '../i18n';
import { useI18n } from '../i18n/useI18n';

/** Display order of the scopes. */
export const HELP_SCOPES: readonly HotkeyScope[] = ['G', 'S', 'T', 'A', 'C', 'L', 'H', 'N', 'X', 'F'];

export interface HelpGroup {
  scope: HotkeyScope;
  entries: KeymapEntry[];
}

/** KEYMAP entries grouped by their first scope, in display order. */
export function helpGroups(keymap: readonly KeymapEntry[] = KEYMAP): HelpGroup[] {
  return HELP_SCOPES.map((scope) => ({ scope, entries: keymap.filter((e) => e.scopes[0] === scope) })).filter(
    (g) => g.entries.length > 0,
  );
}

/** The label key of a KEYMAP id, or null when no area names it. */
export function helpLabelKey(id: string): MessageKey | null {
  const key = `keys.${id}`;
  return isMessageKey(key) ? key : null;
}

export function HelpSheet({ onClose }: { onClose: () => void }) {
  const { t } = useI18n();
  return (
    <Sheet open title={t('help.title')} onCancel={onClose} size="lg" className="help-sheet">
      <div className="help-groups">
        {helpGroups().map((group) => (
          <section key={group.scope} className="help-group" aria-labelledby={`help-${group.scope}`}>
            <h3 id={`help-${group.scope}`} className="help-scope">
              {t(`help.scope.${group.scope}` as MessageKey)}
            </h3>
            <dl className="help-list">
              {group.entries.map((entry) => {
                const key = helpLabelKey(entry.id);
                return (
                  <div key={`${group.scope}-${entry.id}`} className="help-row">
                    <dt>{key ? t(key) : entry.id}</dt>
                    <dd className="help-keys">
                      {entry.combos.length > 4 ? (
                        <span className="kbd">
                          {formatCombo(entry.combos[0] as string)}…{formatCombo(entry.combos[entry.combos.length - 1] as string)}
                        </span>
                      ) : (
                        entry.combos.map((combo) => (
                          <span key={combo} className="kbd">
                            {formatCombo(combo)}
                          </span>
                        ))
                      )}
                    </dd>
                  </div>
                );
              })}
            </dl>
          </section>
        ))}
      </div>
    </Sheet>
  );
}
