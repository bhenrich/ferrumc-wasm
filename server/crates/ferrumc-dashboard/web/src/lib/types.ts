// Mirror of `ferrumc_observability::ServerSnapshot` (serde default snake_case).
// Kept in lock-step with crates/ferrumc-observability/src/snapshot.rs.

export interface Vec3 {
  x: number;
  y: number;
  z: number;
}

export interface ChunkPos {
  x: number;
  z: number;
}

export interface PlayerSnapshot {
  /** u128 UUID; arrives as a JSON number — treat as opaque, key by String(). */
  player_id: number | string;
  name: string;
  position: Vec3;
  chunk: ChunkPos;
  gamemode: string;
  outbound_queue_len: number;
  network_in_bytes: number;
  network_out_bytes: number;
  frames_decoded: number;
  frames_encoded: number;
  packets_dropped_total: number;
}

export interface NetworkMetricsSnapshot {
  player_name: string;
  frames_in: number;
  bytes_in: number;
  frames_out: number;
  bytes_out: number;
  over_budget: number;
  dropped: [number, number, number, number];
}

export interface PluginDecisions {
  allow: number;
  deny: number;
  replace: number;
  panic: number;
}

export interface PluginDecisionSnapshot {
  plugin_name: string;
  decisions: PluginDecisions;
}

export interface MutationCount {
  accepted: number;
  rejected: number;
}

export interface DecodeErrorSnapshot {
  state: string;
  packet: string;
  count: number;
}

export interface PacketFrequency {
  packet_name: string;
  state: string;
  count: number;
}

export interface PacketTraceSummary {
  top_packets: PacketFrequency[];
}

export interface ServerSnapshot {
  build: string;
  started_at: number;
  uptime_secs: number;
  tick: number;

  tps: number;
  tick_p50_ms: number;
  tick_p95_ms: number;
  tick_p99_ms: number;

  players_online: number;
  players: PlayerSnapshot[];

  chunks_loaded: number;
  chunks_dirty: number;
  chunks_persist_dirty: number;
  chunk_sent_total: number;
  chunk_unloaded_total: number;

  network_outbound_queue_len_max: number;
  network_per_player: NetworkMetricsSnapshot[];

  storage_flush_ms_last: number;
  storage_flush_ms_avg: number;

  block_breaks: MutationCount;
  block_places: MutationCount;

  decode_errors_recent: DecodeErrorSnapshot[];
  decode_errors_overflow: number;

  plugin_decisions: PluginDecisionSnapshot[];

  inbound_trace_summary: PacketTraceSummary;
  outbound_trace_summary: PacketTraceSummary;
}

export function emptySnapshot(): ServerSnapshot {
  return {
    build: '',
    started_at: 0,
    uptime_secs: 0,
    tick: 0,
    tps: 0,
    tick_p50_ms: 0,
    tick_p95_ms: 0,
    tick_p99_ms: 0,
    players_online: 0,
    players: [],
    chunks_loaded: 0,
    chunks_dirty: 0,
    chunks_persist_dirty: 0,
    chunk_sent_total: 0,
    chunk_unloaded_total: 0,
    network_outbound_queue_len_max: 0,
    network_per_player: [],
    storage_flush_ms_last: 0,
    storage_flush_ms_avg: 0,
    block_breaks: { accepted: 0, rejected: 0 },
    block_places: { accepted: 0, rejected: 0 },
    decode_errors_recent: [],
    decode_errors_overflow: 0,
    plugin_decisions: [],
    inbound_trace_summary: { top_packets: [] },
    outbound_trace_summary: { top_packets: [] }
  };
}

export type ConnState = 'connecting' | 'live' | 'reconnecting' | 'polling';
