/**
 * Sparkline over the last N samples — the session node's output rate
 * (`NodeMetrics.outputRate`, 60 samples).
 *
 * Pure SVG polyline: no library, no animation, no fill gradient. It answers
 * "how is it running", which is the point of the graph view.
 *
 * BACKLOG 16: the line used to be `currentColor`, so it inherited whatever the
 * surrounding row happened to be — on a `--n8` meta row against `--n1` it was a
 * hairline nobody could see. It now draws in `--n9`, well above the `--n5`
 * floor on both grounds, and the tone never changes: a sparkline is a shape,
 * not a status (UI-GUIDELINES §2-1a).
 */

/** The stroke token. Kept here so both call sites cannot drift apart. */
const SPARKLINE_STROKE = 'var(--n9)';

export interface SparklineProps {
  values: readonly number[];
  width?: number;
  height?: number;
  /** Fixed upper bound; defaults to the max of `values`. */
  max?: number;
  label?: string;
}

export function Sparkline({ values, width = 64, height = 16, max, label }: SparklineProps) {
  if (values.length === 0) {
    return <svg className="sparkline" width={width} height={height} aria-hidden="true" />;
  }

  const peak = Math.max(max ?? 0, ...values, 1);
  const step = values.length > 1 ? width / (values.length - 1) : width;
  const points = values
    .map((v, i) => {
      const x = i * step;
      const y = height - (Math.max(0, v) / peak) * (height - 1) - 0.5;
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(' ');

  return (
    <svg
      className="sparkline"
      width={width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
      preserveAspectRatio="none"
      role="img"
      aria-label={label}
    >
      <polyline
        points={points}
        fill="none"
        stroke={SPARKLINE_STROKE}
        strokeWidth="1"
        strokeLinejoin="round"
        strokeLinecap="round"
      />
    </svg>
  );
}
