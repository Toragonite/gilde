/**
 * The palette (ux-ia §7.4): a trapped dialog with a named combobox; typing
 * filters entities and commands in either language; ⏎ runs the active entry
 * (never during IME composition); a disabled command stays listed with its
 * reason and does not run.
 */

import { act, fireEvent, render } from '@testing-library/react';

import type { ProjectId } from '../lib/gen/ProjectId';
import { appStore } from '../store/useAppStore';
import { pending, project, queueItem, session, snapshot, uid } from '../test/fixtures';
import { assert, assertEqual, domTest } from '../test/harness';
import { press, scenario, seed } from '../shell/testing';
import { Palette } from './Palette';

function world() {
  return snapshot(1, {
    projects: [project('checkout', { liveSessions: 1 })],
    sessions: [
      session(1, {
        projectId: 'checkout' as ProjectId,
        title: '결제 재시도 정리',
        activity: { state: { kind: 'needsApproval' }, since: 0, confidence: 'high', evidence: null, episode: 'e1' as never },
        pending: [pending('r1')],
      }),
    ],
    queue: { rev: 1, items: [queueItem(1, 1)], needYou: 1, also: 0, pausedUntil: null },
  });
}

function input(): HTMLInputElement {
  const el = document.querySelector<HTMLInputElement>('[role="dialog"] input[role="combobox"]');
  if (!el) throw new Error('no palette input');
  return el;
}

domTest(
  'the palette is a named dialog; focus starts in its combobox',
  scenario(() => {
    seed(world());
    render(<Palette />);
    const dialog = document.querySelector('[role="dialog"]');
    assert(dialog?.getAttribute('aria-labelledby'), 'named');
    assertEqual(document.activeElement, input(), 'focus in the input');
    assert(input().getAttribute('aria-label'), 'the input has a name');
  }),
);

domTest(
  'typing a half-composed Korean syllable finds the session; ⏎ opens it',
  scenario(() => {
    seed(world());
    act(() => appStore.getState().setPaletteOpen(true));
    render(<Palette />);
    fireEvent.change(input(), { target: { value: '결제 ㅈ' } });
    const options = [...document.querySelectorAll('[role="option"]')];
    assert(options[0]?.textContent?.includes('결제 재시도 정리') ?? false, 'the session first');
    fireEvent.keyDown(input(), { code: 'Enter', key: 'Enter', isComposing: true });
    assertEqual(appStore.getState().ui.view.screen, 'home', '⏎ during composition does nothing');
    fireEvent.keyDown(input(), { code: 'Enter', key: 'Enter' });
    const view = appStore.getState().ui.view;
    assertEqual(view.screen === 'session' ? view.uid : null, uid(1), 'opened');
    assertEqual(appStore.getState().ui.paletteOpen, false, 'and the palette closed');
  }),
);

domTest(
  'commands match their Korean and English names; > narrows to commands',
  scenario(() => {
    seed(world());
    render(<Palette />);
    fireEvent.change(input(), { target: { value: '>작업장' } });
    const first = document.querySelector('[role="option"]');
    assert(first?.textContent?.includes('Floor') ?? false, 'the English UI finds "Floor" by its Korean name');
    fireEvent.keyDown(input(), { code: 'Enter', key: 'Enter' });
    assertEqual(appStore.getState().ui.view.screen, 'floor', 'runs it');
  }),
);

domTest(
  'a disabled command stays listed with its reason and does not run',
  scenario(() => {
    seed(world());
    render(<Palette />);
    fireEvent.change(input(), { target: { value: '>clean up' } });
    const option = document.querySelector('[role="option"]');
    assertEqual(option?.getAttribute('aria-disabled'), 'true', 'disabled');
    assert(option?.textContent?.includes('No leftover processes.') ?? false, 'with its reason');
    fireEvent.keyDown(input(), { code: 'Enter', key: 'Enter' });
    assertEqual(appStore.getState().ui.sheet, null, 'nothing opened');
  }),
);

domTest(
  'Esc closes the palette',
  scenario(() => {
    seed(world());
    act(() => appStore.getState().setPaletteOpen(true));
    render(<Palette />);
    press('Escape');
    assertEqual(appStore.getState().ui.paletteOpen, false, 'closed');
  }),
);

domTest(
  'A26.2: "New session in {project}" passes the project to the sheet',
  scenario(() => {
    seed(world());
    render(<Palette />);
    fireEvent.change(input(), { target: { value: '>checkout' } });
    const option = [...document.querySelectorAll('[role="option"]')].find((o) => o.textContent?.includes('New session in checkout'));
    assert(option, 'the command');
    fireEvent.click(option);
    assertEqual(appStore.getState().ui.sheet, 'newSession', 'the sheet');
    assertEqual(appStore.getState().ui.sheetContext?.projectId, 'checkout', 'with the project');
  }),
);
