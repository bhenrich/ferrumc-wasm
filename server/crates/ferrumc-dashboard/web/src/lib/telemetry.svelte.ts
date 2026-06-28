// Live telemetry store: seeds from GET /api/snapshot, then streams GET /events
// (Server-Sent Events) and degrades to 1 Hz polling if the stream stalls or
// EventSource is unavailable. Exposes a reactive `snapshot` + connection state
// for the UI, and a plain ring-buffered history (bumped via `rev`) for charts.

import { emptySnapshot, type ConnState, type ServerSnapshot } from './types';

const HISTORY_CAP = 300; // ~30 s at the 10 Hz stream cadence
const STALL_MS = 3000; // no SSE frame for this long ⇒ fall back to polling
const POLL_MS = 1000;

export interface History {
  t: number[];
  tps: number[];
  p50: number[];
  p95: number[];
  p99: number[];
  flush: number[];
  qmax: number[];
}

class Telemetry {
  snapshot = $state<ServerSnapshot>(emptySnapshot());
  conn = $state<ConnState>('connecting');
  /** Bumped on every applied snapshot so charts can pull from `history`. */
  rev = $state(0);
  /** Wall-clock ms of the most recent applied snapshot (0 until first frame). */
  lastUpdate = $state(0);

  // Plain (non-reactive) ring buffer; charts read it imperatively on `rev`.
  history: History = { t: [], tps: [], p50: [], p95: [], p99: [], flush: [], qmax: [] };

  #t0 = 0;
  #source: EventSource | null = null;
  #pollTimer: ReturnType<typeof setInterval> | null = null;
  #watchdog: ReturnType<typeof setInterval> | null = null;
  #started = false;

  /** Begin acquiring data. Safe to call once, from the browser only. */
  start(): void {
    if (this.#started) return;
    this.#started = true;
    void this.#seed();
    this.#openStream();
    this.#watchdog = setInterval(() => this.#checkStall(), 1000);
  }

  /** Tear everything down (component unmount / HMR). */
  stop(): void {
    this.#source?.close();
    this.#source = null;
    this.#stopPolling();
    if (this.#watchdog) clearInterval(this.#watchdog);
    this.#watchdog = null;
    this.#started = false;
  }

  async #seed(): Promise<void> {
    try {
      const res = await fetch('/api/snapshot', { cache: 'no-store' });
      if (res.ok) this.#apply(await res.json());
    } catch {
      /* stream/poll will recover */
    }
  }

  #openStream(): void {
    if (typeof EventSource === 'undefined') {
      this.#startPolling();
      return;
    }
    try {
      const source = new EventSource('/events');
      this.#source = source;
      source.onopen = () => {
        this.#stopPolling();
        this.conn = 'live';
      };
      source.onmessage = (event) => {
        this.#stopPolling();
        try {
          this.#apply(JSON.parse(event.data));
          this.conn = 'live';
        } catch {
          /* ignore a single bad frame */
        }
      };
      source.onerror = () => {
        // EventSource reconnects on its own; reflect the gap and let the
        // watchdog spin up polling if the outage drags on.
        if (this.conn !== 'polling') this.conn = 'reconnecting';
      };
    } catch {
      this.#startPolling();
    }
  }

  #checkStall(): void {
    if (this.lastUpdate === 0) return;
    if (Date.now() - this.lastUpdate > STALL_MS) this.#startPolling();
  }

  #startPolling(): void {
    if (this.#pollTimer) return;
    this.conn = this.#source ? 'reconnecting' : 'polling';
    const tick = async () => {
      try {
        const res = await fetch('/api/snapshot', { cache: 'no-store' });
        if (res.ok) {
          this.#apply(await res.json());
          if (!this.#source) this.conn = 'polling';
        }
      } catch {
        /* keep trying */
      }
    };
    void tick();
    this.#pollTimer = setInterval(tick, POLL_MS);
  }

  #stopPolling(): void {
    if (this.#pollTimer) clearInterval(this.#pollTimer);
    this.#pollTimer = null;
  }

  #apply(snap: ServerSnapshot): void {
    this.snapshot = snap;
    const now = Date.now();
    this.lastUpdate = now;
    if (this.#t0 === 0) this.#t0 = now;
    const t = (now - this.#t0) / 1000;
    const h = this.history;
    h.t.push(t);
    h.tps.push(snap.tps);
    h.p50.push(snap.tick_p50_ms);
    h.p95.push(snap.tick_p95_ms);
    h.p99.push(snap.tick_p99_ms);
    h.flush.push(snap.storage_flush_ms_last);
    h.qmax.push(snap.network_outbound_queue_len_max);
    if (h.t.length > HISTORY_CAP) {
      for (const key of Object.keys(h) as (keyof History)[]) h[key].shift();
    }
    this.rev++;
  }
}

export const telemetry = new Telemetry();
