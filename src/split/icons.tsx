/**
 * The tile header's own icons — inline Lucide (MIT) paths in the same shape as
 * `components/icons.tsx` (16 px, stroke 1.5, `currentColor`, aria-hidden).
 * They live here because that file is frozen for this task; the class names
 * are the shared ones, so they size and colour like every other icon.
 */

import type { SVGProps } from 'react';

interface Props extends SVGProps<SVGSVGElement> {
  size?: 'sm' | 'md';
}

function Svg({ size = 'sm', className, children, ...rest }: Props) {
  return (
    <svg
      viewBox="0 0 24 24"
      className={['i', size === 'sm' ? 'sm' : '', className ?? ''].filter(Boolean).join(' ')}
      aria-hidden="true"
      focusable="false"
      {...rest}
    >
      {children}
    </svg>
  );
}

/** Lucide `maximize-2`. */
export function ZoomIcon(props: Props) {
  return (
    <Svg {...props}>
      <polyline points="15 3 21 3 21 9" />
      <polyline points="9 21 3 21 3 15" />
      <line x1="21" y1="3" x2="14" y2="10" />
      <line x1="3" y1="21" x2="10" y2="14" />
    </Svg>
  );
}

/** Lucide `minimize-2`. */
export function RestoreIcon(props: Props) {
  return (
    <Svg {...props}>
      <polyline points="4 14 10 14 10 20" />
      <polyline points="20 10 14 10 14 4" />
      <line x1="14" y1="10" x2="21" y2="3" />
      <line x1="3" y1="21" x2="10" y2="14" />
    </Svg>
  );
}

/** Lucide `arrow-up-right`: open the full Session screen. */
export function OpenIcon(props: Props) {
  return (
    <Svg {...props}>
      <line x1="7" y1="17" x2="17" y2="7" />
      <polyline points="7 7 17 7 17 17" />
    </Svg>
  );
}
