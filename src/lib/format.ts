/**
 * Locale-aware formatting, all of it through `Intl` (DESIGN §9-9).
 *
 * `t()` only interpolates `{name}`; every number, byte count, duration, rate,
 * cost and date is rendered here so that Korean and English each get their own
 * conventions instead of one hand-rolled compromise that is wrong in both.
 *
 * Byte counts use decimal units (1 MB = 10^6 B) to match the unit names `Intl`
 * prints and the numbers macOS itself reports.
 */

import type { Locale } from './ipc';

// ---------------------------------------------------------------------------
// Formatter cache — constructing an Intl formatter is expensive and these run
// at 10 Hz on the status bar.
// ---------------------------------------------------------------------------

const numberCache = new Map<string, Intl.NumberFormat>();
const relativeCache = new Map<string, Intl.RelativeTimeFormat>();
const dateCache = new Map<string, Intl.DateTimeFormat>();

function numberFormat(locale: Locale, options: Intl.NumberFormatOptions): Intl.NumberFormat {
  const key = locale + JSON.stringify(options);
  let f = numberCache.get(key);
  if (!f) {
    f = new Intl.NumberFormat(locale, options);
    numberCache.set(key, f);
  }
  return f;
}

function relativeFormat(locale: Locale): Intl.RelativeTimeFormat {
  let f = relativeCache.get(locale);
  if (!f) {
    f = new Intl.RelativeTimeFormat(locale, { numeric: 'auto', style: 'short' });
    relativeCache.set(locale, f);
  }
  return f;
}

function dateFormat(locale: Locale, options: Intl.DateTimeFormatOptions): Intl.DateTimeFormat {
  const key = locale + JSON.stringify(options);
  let f = dateCache.get(key);
  if (!f) {
    f = new Intl.DateTimeFormat(locale, options);
    dateCache.set(key, f);
  }
  return f;
}

// ---------------------------------------------------------------------------
// Numbers
// ---------------------------------------------------------------------------

export function formatNumber(value: number, locale: Locale, fractionDigits = 0): string {
  if (!Number.isFinite(value)) return '—';
  return numberFormat(locale, {
    maximumFractionDigits: fractionDigits,
    minimumFractionDigits: 0,
  }).format(value);
}

/** `value` is a fraction in 0..1. `formatPercent(0.41)` → `41%`. */
export function formatPercent(value: number, locale: Locale, fractionDigits = 0): string {
  if (!Number.isFinite(value)) return '—';
  return numberFormat(locale, {
    style: 'percent',
    maximumFractionDigits: fractionDigits,
  }).format(value);
}

/** `value` is already 0..100, as `Totals.rssBudgetPercent` reports it. */
export function formatPercentPoints(value: number, locale: Locale, fractionDigits = 0): string {
  return formatPercent(value / 100, locale, fractionDigits);
}

export function formatCostUsd(value: number, locale: Locale): string {
  if (!Number.isFinite(value)) return '—';
  return numberFormat(locale, {
    style: 'currency',
    currency: 'USD',
    maximumFractionDigits: value < 1 ? 3 : 2,
  }).format(value);
}

// ---------------------------------------------------------------------------
// Bytes
// ---------------------------------------------------------------------------

const BYTE_UNITS = ['byte', 'kilobyte', 'megabyte', 'gigabyte', 'terabyte'] as const;

export function formatBytes(bytes: number, locale: Locale): string {
  if (!Number.isFinite(bytes)) return '—';
  const sign = bytes < 0 ? -1 : 1;
  let value = Math.abs(bytes);
  let unit = 0;
  while (value >= 1000 && unit < BYTE_UNITS.length - 1) {
    value /= 1000;
    unit += 1;
  }
  const digits = unit === 0 ? 0 : value < 10 ? 1 : 0;
  return numberFormat(locale, {
    style: 'unit',
    unit: BYTE_UNITS[unit],
    unitDisplay: 'short',
    maximumFractionDigits: digits,
    minimumFractionDigits: 0,
  }).format(sign * value);
}

/** Output rate for a session sparkline: bytes per second. */
export function formatByteRate(bytesPerSecond: number, locale: Locale): string {
  return `${formatBytes(bytesPerSecond, locale)}/s`;
}

// ---------------------------------------------------------------------------
// Durations
// ---------------------------------------------------------------------------

const SECOND = 1000;
const MINUTE = 60 * SECOND;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/**
 * A humanized, localized duration using the largest unit that fits:
 * ko `4.2초` / `12분` / `2일`, en `4.2 sec` / `12 min` / `2 days`.
 */
export function formatDuration(ms: number, locale: Locale): string {
  if (!Number.isFinite(ms)) return '—';
  const abs = Math.abs(ms);
  let value: number;
  let unit: 'second' | 'minute' | 'hour' | 'day';
  let digits = 0;
  if (abs < MINUTE) {
    value = ms / SECOND;
    unit = 'second';
    digits = abs < 10 * SECOND ? 1 : 0;
  } else if (abs < HOUR) {
    value = ms / MINUTE;
    unit = 'minute';
  } else if (abs < DAY) {
    value = ms / HOUR;
    unit = 'hour';
    digits = abs < 10 * HOUR ? 1 : 0;
  } else {
    value = ms / DAY;
    unit = 'day';
    digits = abs < 10 * DAY ? 1 : 0;
  }
  return numberFormat(locale, {
    style: 'unit',
    unit,
    unitDisplay: 'short',
    maximumFractionDigits: digits,
    minimumFractionDigits: 0,
  }).format(value);
}

/**
 * Monospace clock elapsed time for table columns and pane headers:
 * `4:12`, `1:04:11`, `2d 04:11`. Digits only, so it is locale independent by
 * design — it lines up under `font-variant-numeric: tabular-nums`.
 */
export function formatElapsedClock(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return '—';
  const total = Math.floor(ms / SECOND);
  const days = Math.floor(total / 86400);
  const hours = Math.floor((total % 86400) / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const seconds = total % 60;
  const pad = (n: number) => String(n).padStart(2, '0');
  if (days > 0) return `${days}d ${pad(hours)}:${pad(minutes)}`;
  if (hours > 0) return `${hours}:${pad(minutes)}:${pad(seconds)}`;
  return `${minutes}:${pad(seconds)}`;
}

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

/** `Intl.RelativeTimeFormat`: ko `3분 전`, en `3 min. ago`. */
export function formatRelativeTime(at: number, locale: Locale, now = Date.now()): string {
  if (!Number.isFinite(at)) return '—';
  const delta = at - now;
  const abs = Math.abs(delta);
  const f = relativeFormat(locale);
  if (abs < MINUTE) return f.format(Math.round(delta / SECOND), 'second');
  if (abs < HOUR) return f.format(Math.round(delta / MINUTE), 'minute');
  if (abs < DAY) return f.format(Math.round(delta / HOUR), 'hour');
  return f.format(Math.round(delta / DAY), 'day');
}

/** Wall clock time of day, e.g. the session start row in the inspector. */
export function formatTimeOfDay(at: number, locale: Locale): string {
  if (!Number.isFinite(at)) return '—';
  return dateFormat(locale, {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hourCycle: 'h23',
  }).format(new Date(at));
}

export function formatDateTime(at: number, locale: Locale): string {
  if (!Number.isFinite(at)) return '—';
  return dateFormat(locale, { dateStyle: 'short', timeStyle: 'medium' }).format(new Date(at));
}

// ---------------------------------------------------------------------------
// Small helpers shared by several panels
// ---------------------------------------------------------------------------

/** `+412 −38` — the diff summary used in the sidebar, graph and inspector. */
export function formatDiffStat(added: number, removed: number, locale: Locale): string {
  return `+${formatNumber(added, locale)} −${formatNumber(removed, locale)}`;
}

/** Shorten a long absolute path to `…/tail/segments` for narrow columns. */
export function truncatePath(path: string, segments = 2): string {
  const parts = path.split('/').filter(Boolean);
  if (parts.length <= segments) return path;
  return `…/${parts.slice(-segments).join('/')}`;
}
