/**
 * 1–2px meter bar. Colour only appears at a threshold (UI-GUIDELINES §2-1a);
 * a healthy bar is neutral, so a coloured bar always means something.
 */

export interface BarProps {
  /** 0..1. Values outside the range are clamped. */
  value: number;
  /** Fraction above which the bar turns amber. */
  warnAt?: number;
  /** Fraction above which the bar turns red. */
  criticalAt?: number;
  label?: string;
}

export function Bar({ value, warnAt = 0.7, criticalAt = 0.85, label }: BarProps) {
  const clamped = Number.isFinite(value) ? Math.min(1, Math.max(0, value)) : 0;
  const tone = clamped >= criticalAt ? 'error' : clamped >= warnAt ? 'warn' : undefined;
  return (
    <div
      className="bar"
      role="meter"
      aria-valuenow={Math.round(clamped * 100)}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-label={label}
      title={label}
    >
      <span style={{ width: `${clamped * 100}%` }} data-tone={tone} />
    </div>
  );
}
