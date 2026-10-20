/**
 * Unit test entry point. `src/test/run.mjs` bundles this with Vite and runs it
 * on Node; every `test()` registered by the imported modules is executed in
 * import order.
 */

import './env';
import { runAll } from './harness';

import '../i18n/i18n.test';
import '../lib/codec.test';
import '../lib/format.test';
import '../hooks/useHotkeys.test';
import '../store/graphSlice.test';
import '../store/uiSlice.test';
import '../terminal/terminal.test'; // T20
import '../terminal/statusEvidence.test'; // M1.5 B-status (BACKLOG 4, 12)
import '../sidebar/sidebar.test'; // T21
import '../dispatch/dispatch.test'; // T24
import '../monitor/monitor.test'; // T23
import '../graph/graph.test'; // T25
import '../shell/shell.test'; // M1.5 C-shell (BACKLOG 3, 6, 9)
import '../shell/health.test'; // the health surface (indicator, popover, alerts)
import '../lib/notify.test'; // system notifications for health alerts
import '../dialogs/dialogs.test'; // T26
import '../palette/palette.test'; // T27
import '../team/team.test'; // T8 team preview (read-only)
import '../team/grouping.test'; // Team view scope + worktree-aware repository grouping
import '../team/livecheck.test'; // live check G-a … G-d (B1–B4)
import '../workers/workers.test'; // plan #3c worker accounts (read-only)
import './render.test';
import './preview.test'; // the preview build: lanes hidden, Team first
import './showcase.test'; // the showcase: demo scenes, no Tauri (last: it seeds the shared store)

export async function main(): Promise<number> {
  const lines: string[] = [];
  const summary = await runAll((line) => lines.push(line));
  for (const line of lines) console.log(line);
  console.log(`\n${summary.passed} passed, ${summary.failed} failed`);
  return summary.failed === 0 ? 0 : 1;
}
