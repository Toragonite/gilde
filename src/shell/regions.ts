/**
 * F6 / ⇧F6 region cycling (ux-ia §7.2, §7.5): sidebar → main → composer
 * (→ the next tile in Split/Team). A region is any element marked
 * `data-region`; screens mark their composer (`data-region="composer"`) and
 * tiles themselves. Focus lands on the region's `[data-region-focus]`
 * element, else its active roving item (`tabindex="0"`), else its first
 * focusable control, else the region itself.
 */

const FOCUSABLE =
  'a[href],button:not([disabled]),input:not([disabled]):not([type="hidden"]),select:not([disabled]),textarea:not([disabled]),[tabindex]:not([tabindex="-1"])';

function visible(el: HTMLElement): boolean {
  if (el.closest('[hidden],[inert],[aria-hidden="true"]')) return false;
  return true;
}

export function regions(root: ParentNode = document): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>('[data-region]')).filter(visible);
}

function focusTarget(region: HTMLElement): HTMLElement {
  const marked = region.querySelector<HTMLElement>('[data-region-focus]');
  if (marked && visible(marked)) return marked;
  const roving = region.querySelector<HTMLElement>('[tabindex="0"]');
  if (roving && visible(roving)) return roving;
  const first = Array.from(region.querySelectorAll<HTMLElement>(FOCUSABLE)).find(visible);
  if (first) return first;
  if (!region.hasAttribute('tabindex')) region.setAttribute('tabindex', '-1');
  return region;
}

/** Move focus to the next (1) or previous (-1) region; returns the region focused. */
export function cycleRegion(dir: 1 | -1, root: ParentNode = document): HTMLElement | null {
  const list = regions(root);
  if (list.length === 0) return null;
  const active = document.activeElement;
  // The innermost region holding focus (a tile inside main counts as itself).
  let at = -1;
  list.forEach((r, i) => {
    if (active instanceof Node && r.contains(active)) at = i;
  });
  const next = list[(at + dir + list.length) % list.length] ?? list[0];
  if (!next) return null;
  focusTarget(next).focus();
  return next;
}
