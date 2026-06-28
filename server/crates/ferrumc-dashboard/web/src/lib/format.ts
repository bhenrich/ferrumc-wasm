// Small, dependency-free formatters for the console's numerals.

/** Heat class from TPS: green ≥19, amber ≥15, red below. */
export function tpsHeat(tps: number): 'ok' | 'warn' | 'bad' {
  if (tps >= 19) return 'ok';
  if (tps >= 15) return 'warn';
  return 'bad';
}

/** 0…1 "molten" intensity from a value approaching a budget (e.g. MSPT→50ms). */
export function heatRatio(value: number, budget: number): number {
  if (budget <= 0) return 0;
  return Math.max(0, Math.min(1, value / budget));
}

export function fmtInt(n: number): string {
  return Math.round(n).toLocaleString('en-US');
}

export function fmt1(n: number): string {
  return n.toFixed(1);
}

export function fmt2(n: number): string {
  return n.toFixed(2);
}

/** Compact byte sizes: 1.4 KiB, 23 MiB. */
export function fmtBytes(n: number): string {
  if (!n) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  const i = Math.min(units.length - 1, Math.floor(Math.log(n) / Math.log(1024)));
  const v = n / Math.pow(1024, i);
  return `${i === 0 ? v : v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

/** Seconds → 1d 02h 03m 04s, trimmed to the two most significant units. */
export function fmtUptime(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  if (d) return `${d}d ${h.toString().padStart(2, '0')}h`;
  if (h) return `${h}h ${m.toString().padStart(2, '0')}m`;
  if (m) return `${m}m ${sec.toString().padStart(2, '0')}s`;
  return `${sec}s`;
}

export function pid(value: number | string): string {
  return String(value);
}
