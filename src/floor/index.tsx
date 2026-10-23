/**
 * The Floor / 작업장 (contract §15.2 T17, M9, M16).
 *
 * Mount points (A23.2; the shell finds this module by its `src/<area>/index.{ts,tsx}` glob, A23.5):
 *  - `<FloorScreen />` — the global Floor (main view `{ screen: 'floor' }`);
 *  - `<TeamFloor teamKey={…} />` — the body of Team › Floor (`⌥⌘3`).
 * Both fill their parent (`height: 100%`, a flex column).
 *
 * Loading the Floor starts the replay ring; showcase builds also get the
 * geometry gate's test hooks (`Floor.tsx`).
 */

export { Floor, FloorScreen, TeamFloor } from './Floor';
