/**
 * The window's width band for Split and Team (ux-ia §2.8, M11), computed with
 * the shell's own rule (`shell/layout.ts`: window width after UI zoom), so the
 * tiles switch layout exactly when the shell hides the sidebar.
 */

import { useEffect, useState } from 'react';

import { effectiveWidth, widthBand } from '../shell/layout';
import type { WidthBand } from '../shell/layout';
import { useAppStore } from '../store/useAppStore';

export function useBand(forced?: WidthBand): WidthBand {
  const zoom = useAppStore((s) => s.ui.zoom);
  const [width, setWidth] = useState(() => (typeof window === 'undefined' ? 1440 : window.innerWidth));
  useEffect(() => {
    if (forced || typeof window === 'undefined') return;
    const onResize = () => setWidth(window.innerWidth);
    window.addEventListener('resize', onResize);
    return () => window.removeEventListener('resize', onResize);
  }, [forced]);
  return forced ?? widthBand(effectiveWidth(width, zoom));
}
