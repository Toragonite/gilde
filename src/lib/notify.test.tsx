/**
 * Live-region announcements (ux-ia §10): keyed announcements once, polite vs
 * assertive regions, the 10 s summary throttle, and the regions themselves.
 */

import { act, render } from '@testing-library/react';

import { LiveRegion } from '../components/LiveRegion';
import { assert, assertEqual, domTest, test } from '../test/harness';
import { manualClock, setClockForTests } from './clock';
import {
  SUMMARY_INTERVAL_MS,
  announce,
  announceSummary,
  getAnnouncements,
  resetAnnouncerForTests,
  tickAnnouncer,
  wasAnnounced,
} from './notify';

function fresh(run: () => void) {
  return () => {
    const c = manualClock(50_000);
    setClockForTests(c);
    resetAnnouncerForTests();
    try {
      run();
    } finally {
      resetAnnouncerForTests();
      setClockForTests(null);
    }
  };
}

test(
  'announce: a keyed announcement is made once (a new card, assertively)',
  fresh(() => {
    assert(announce('api-refactor needs approval', { politeness: 'assertive', key: 'req-1' }), 'first time');
    assertEqual(getAnnouncements().assertive?.text, 'api-refactor needs approval', 'in the assertive region');
    assertEqual(getAnnouncements().polite, null, 'not in the polite one');
    assert(!announce('api-refactor needs approval', { politeness: 'assertive', key: 'req-1' }), 'never twice');
    assert(wasAnnounced('req-1'), 'remembered');
    assert(announce('Request answered elsewhere', { key: 'req-1:withdrawn' }), 'another key');
    assertEqual(getAnnouncements().polite?.text, 'Request answered elsewhere', 'polite by default');
    assert(!announce('   '), 'empty text is not announced');
  }),
);

test(
  'announce: the same text without a key is announced again (new id)',
  fresh(() => {
    announce('claude finished replying');
    const first = getAnnouncements().polite?.id;
    announce('claude finished replying');
    assert(getAnnouncements().polite?.id !== first, 'a new announcement');
  }),
);

test('announceSummary: at most one per 10 s; the newest text wins', () => {
  const c = manualClock(50_000);
  setClockForTests(c);
  resetAnnouncerForTests();
  try {
    announceSummary('Needs you: 1.');
    assertEqual(getAnnouncements().polite?.text, 'Needs you: 1.', 'the first is immediate');
    c.advance(2_000);
    announceSummary('Needs you: 2.');
    c.advance(2_000);
    announceSummary('Needs you: 3.');
    assertEqual(getAnnouncements().polite?.text, 'Needs you: 1.', 'throttled');
    c.advance(SUMMARY_INTERVAL_MS - 4_001);
    tickAnnouncer();
    assertEqual(getAnnouncements().polite?.text, 'Needs you: 1.', 'still inside the 10 s');
    c.advance(1);
    tickAnnouncer();
    assertEqual(getAnnouncements().polite?.text, 'Needs you: 3.', 'the newest pending summary after 10 s');
    c.advance(1_000);
    tickAnnouncer();
    assertEqual(getAnnouncements().polite?.text, 'Needs you: 3.', 'nothing pending, nothing new');
  } finally {
    resetAnnouncerForTests();
    setClockForTests(null);
  }
});

domTest('LiveRegion: a polite status region and an assertive region carry the text', () => {
  resetAnnouncerForTests();
  try {
    const view = render(<LiveRegion />);
    const polite = view.container.querySelector('[data-live="polite"]') as HTMLElement;
    const assertive = view.container.querySelector('[data-live="assertive"]') as HTMLElement;
    assertEqual(polite.getAttribute('role'), 'status', 'polite region is a status');
    assertEqual(polite.getAttribute('aria-live'), 'polite', 'polite');
    assertEqual(assertive.getAttribute('aria-live'), 'assertive', 'assertive');
    assertEqual(polite.getAttribute('aria-atomic'), 'true', 'read whole');
    act(() => {
      announce('Read 6 files');
    });
    assertEqual(polite.textContent, 'Read 6 files', 'polite text shown');
    act(() => {
      announce('db-agent needs approval', { politeness: 'assertive', key: 'r9' });
    });
    assertEqual(assertive.textContent, 'db-agent needs approval', 'assertive text shown');
    act(() => {
      announce('db-agent needs approval', { politeness: 'assertive', key: 'r9' });
    });
    assertEqual(assertive.textContent, 'db-agent needs approval', 'unchanged on a repeat key');
    assert((view.container.firstElementChild as HTMLElement).className.includes('g-sr-only'), 'visually hidden');
  } finally {
    act(() => resetAnnouncerForTests());
  }
});
