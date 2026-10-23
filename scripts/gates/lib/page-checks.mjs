/**
 * The in-page checks of the G4 browser and accessibility gates (contract
 * §14.3), as expressions evaluated in the page. They report counts, rule ids,
 * element tags and classes — never page text (the showcase data is neutral,
 * but the gates must stay safe on any build).
 */

/**
 * Visible focusable controls of the main region whose box intersects the first
 * viewport (Focus Home: ≤ 16, M15). M15 governs Home's content, so the count
 * is scoped to the page's main landmark; the sidebar is navigation with its
 * own rules (A26.6). A page without a main is counted whole.
 */
export const DENSITY = `(() => {
  const sel = 'a[href],button,input,select,textarea,summary,[tabindex]:not([tabindex="-1"]),[role=button],[role=link],[role=tab],[role=menuitem],[role=checkbox],[role=switch],[role=radio],[role=option]';
  const scope = document.querySelector('main,[role=main]') || document;
  const vh = innerHeight;
  const seen = new Set();
  let n = 0;
  for (const el of scope.querySelectorAll(sel)) {
    if (seen.has(el)) continue;
    seen.add(el);
    if (el.closest('[aria-hidden="true"],[inert]') || el.disabled) continue;
    const cs = getComputedStyle(el);
    if (cs.display === 'none' || cs.visibility === 'hidden') continue;
    const r = el.getBoundingClientRect();
    if (r.width === 0 || r.height === 0) continue;
    if (r.top < vh && r.bottom > 0) n++;
  }
  return n;
})()`;

/**
 * Elements whose own text is cut horizontally without an ellipsis (no clipped
 * text at 900 px). Visually hidden text — the .g-sr-only pattern: a box of
 * at most 1×1 px with overflow: hidden and a clip or clip-path, on the
 * element or an ancestor — is never on screen, so it is skipped (A26.4).
 */
export const CLIPPED = `(() => {
  const srOnly = (el) => {
    for (let n = el; n && n !== document.body; n = n.parentElement) {
      const cs = getComputedStyle(n);
      const r = n.getBoundingClientRect();
      const clipped = (cs.clip && cs.clip !== 'auto') || (cs.clipPath && cs.clipPath !== 'none');
      if (r.width <= 1 && r.height <= 1 && /(hidden|clip)/.test(cs.overflow) && clipped) return true;
    }
    return false;
  };
  const out = [];
  for (const el of document.querySelectorAll('body *')) {
    if (![...el.childNodes].some((c) => c.nodeType === 3 && c.textContent.trim())) continue;
    const cs = getComputedStyle(el);
    if (cs.display === 'none' || cs.visibility === 'hidden' || el.closest('[aria-hidden="true"]')) continue;
    if (srOnly(el)) continue;
    if (!/(hidden|clip)/.test(cs.overflowX)) continue;
    if (cs.textOverflow === 'ellipsis' || (cs.webkitLineClamp && cs.webkitLineClamp !== 'none')) continue;
    if (el.scrollWidth > el.clientWidth + 1) out.push((el.tagName.toLowerCase() + (typeof el.className === 'string' && el.className ? '.' + el.className.trim().split(/\\s+/)[0] : '')).slice(0, 60));
  }
  return out.slice(0, 50);
})()`;

/** axe-core (already injected): serious and critical violations as rule ids and node counts. */
export const AXE = `(async () => {
  const r = await window.axe.run(document, { resultTypes: ['violations'] });
  return r.violations.map((v) => ({ id: v.id, impact: v.impact, nodes: v.nodes.length }));
})()`;

/** Accessibility-tree facts of ux-ia §10 that axe does not phrase this way. */
export const ARIA = `(() => {
  const named = (el) => (el.getAttribute('aria-label') || '').trim() || el.getAttribute('aria-labelledby') || (el.labels && el.labels.length) || el.getAttribute('title');
  const inputs = [...document.querySelectorAll('input:not([type=hidden]),select,textarea,[role=textbox],[role=combobox],[role=searchbox]')].filter((el) => !el.closest('[aria-hidden="true"]'));
  const landmarks = [...document.querySelectorAll('main,nav,aside,header,footer,[role=main],[role=navigation],[role=complementary],[role=region][aria-label]')]
    .map((el) => (el.getAttribute('role') || el.tagName.toLowerCase()) + '|' + (el.getAttribute('aria-label') || el.getAttribute('aria-labelledby') || ''));
  return {
    unnamedInputs: inputs.filter((el) => !named(el)).length,
    duplicateLandmarks: landmarks.length - new Set(landmarks).size,
    mains: document.querySelectorAll('main,[role=main]').length,
    approvalGroup: !!document.querySelector('[role="group"][aria-keyshortcuts], [role="group"] [aria-keyshortcuts]'),
    assertiveRegions: document.querySelectorAll('[aria-live="assertive"]').length,
  };
})()`;

/** The Floor's geometry hook (pixel-office, ported by T17): null when this build has no Floor. */
export const FLOOR = `(() => (typeof window.__flOverlaps === 'function' ? window.__flOverlaps() : null))()`;

/** Where focus is: inside the open dialog or sheet, and whether one is open. */
export const FOCUS = `(() => {
  const d = document.querySelector('[role="dialog"],[role="alertdialog"],dialog[open]');
  return { dialog: !!d, focusInside: !!(d && d.contains(document.activeElement)), focusables: d ? d.querySelectorAll('a[href],button:not([disabled]),input:not([disabled]),select,textarea,[tabindex]:not([tabindex="-1"])').length : 0 };
})()`;

/**
 * The open modal sheet (`aria-modal="true"`; a non-modal dialog such as the
 * Recovered dialog is not one), whether focus is inside it, and the focused
 * control's tag when focus left the body.
 */
export const MODAL = `(() => {
  const d = document.querySelector('[role="dialog"][aria-modal="true"],[role="alertdialog"][aria-modal="true"],dialog[open]');
  const a = document.activeElement;
  return {
    modal: !!d,
    inside: !!(d && d.contains(a)),
    focusables: d ? d.querySelectorAll('a[href],button:not([disabled]),input:not([disabled]),select,textarea,[tabindex]:not([tabindex="-1"])').length : 0,
    active: a && a !== document.body && a !== document.documentElement ? a.tagName.toLowerCase() + (a.getAttribute('role') ? '[' + a.getAttribute('role') + ']' : '') : null,
  };
})()`;

export const READY = `document.documentElement.getAttribute('data-showcase-ready')`;
