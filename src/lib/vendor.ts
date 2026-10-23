/**
 * How a vendor is named in the UI: the lowercase CLI badge
 * (`claude · codex · cursor · agy`, ux-ia §9.1 Product names).
 */

import type { Vendor } from './gen/Vendor';

const BADGES: Readonly<Record<Vendor, string>> = {
  claude: 'claude',
  codex: 'codex',
  cursor: 'cursor',
  agy: 'agy',
};

export const VENDORS: readonly Vendor[] = ['claude', 'codex', 'cursor', 'agy'];

/** The badge text of a vendor; an unknown id is shown as it is, never hidden. */
export function vendorBadge(vendor: Vendor | string | null | undefined): string {
  if (typeof vendor !== 'string' || vendor === '') return '';
  return (BADGES as Record<string, string>)[vendor] ?? vendor;
}
