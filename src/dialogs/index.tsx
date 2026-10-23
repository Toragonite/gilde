/**
 * Dialogs (T18) that are not sheets, mounted once at the shell root as
 * `Dialogs()` (contract A23.5): the Recovered dialog (§10.2–§10.3). Settings
 * is a main view (`src/settings`) and the keyboard help is the shell's.
 */

import { RecoveredDialog } from './RecoveredDialog';
import './dialogs.css';

export { RecoveredDialog, recoveredLeftovers } from './RecoveredDialog';

export function Dialogs() {
  return <RecoveredDialog />;
}
