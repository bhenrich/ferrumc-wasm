//! [`PlayMetrics`]: placeholder per-connection play counters.
//!
//! These are plain in-process counters, **not** wired to any metrics backend.
//! They exist so the reader/writer can record activity (frames, bytes, drops,
//! over-budget events) in a shape a later observability milestone can export.
//! The [`PlayReader`](crate::PlayReader) and [`PlayWriter`](crate::PlayWriter)
//! each own their own instance — matching the reader/writer task split — so each
//! half records only the counters relevant to it without shared mutable state.

use super::priority::{OutboundPriority, PRIORITY_COUNT};

/// Per-connection play-loop counters.
///
/// All counters saturate rather than wrap, so a long-lived connection cannot
/// panic on overflow. Read them through the accessor methods; the internal
/// layout is not part of the API.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayMetrics {
    frames_decoded: u64,
    bytes_in: u64,
    over_budget: u64,
    frames_encoded: u64,
    bytes_out: u64,
    batches_flushed: u64,
    dropped: [u64; PRIORITY_COUNT],
}

impl PlayMetrics {
    /// Creates a zeroed counter set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Serverbound frames successfully decoded into typed packets.
    pub fn frames_decoded(&self) -> u64 {
        self.frames_decoded
    }

    /// Total serverbound frame-body bytes decoded.
    pub fn bytes_in(&self) -> u64 {
        self.bytes_in
    }

    /// Serverbound frames classified over the packet budget.
    pub fn over_budget(&self) -> u64 {
        self.over_budget
    }

    /// Clientbound frames encoded into outbound batches.
    pub fn frames_encoded(&self) -> u64 {
        self.frames_encoded
    }

    /// Total clientbound on-wire bytes produced (post-framing, post-compression).
    pub fn bytes_out(&self) -> u64 {
        self.bytes_out
    }

    /// Non-empty outbound batches drained.
    pub fn batches_flushed(&self) -> u64 {
        self.batches_flushed
    }

    /// Clientbound packets dropped because `priority`'s queue was full.
    pub fn dropped(&self, priority: OutboundPriority) -> u64 {
        self.dropped[priority.index()]
    }

    /// Clientbound packets dropped across all priority queues.
    pub fn dropped_total(&self) -> u64 {
        self.dropped
            .iter()
            .fold(0u64, |acc, n| acc.saturating_add(*n))
    }

    /// Records one decoded serverbound frame of `bytes` body length.
    pub(crate) fn record_frame_decoded(&mut self, bytes: usize) {
        self.frames_decoded = self.frames_decoded.saturating_add(1);
        self.bytes_in = self.bytes_in.saturating_add(bytes as u64);
    }

    /// Records one serverbound frame classified over budget.
    pub(crate) fn record_over_budget(&mut self) {
        self.over_budget = self.over_budget.saturating_add(1);
    }

    /// Records one encoded clientbound frame of `bytes` on-wire length.
    pub(crate) fn record_frame_encoded(&mut self, bytes: usize) {
        self.frames_encoded = self.frames_encoded.saturating_add(1);
        self.bytes_out = self.bytes_out.saturating_add(bytes as u64);
    }

    /// Records one non-empty outbound batch drain.
    pub(crate) fn record_batch(&mut self) {
        self.batches_flushed = self.batches_flushed.saturating_add(1);
    }

    /// Records one clientbound packet dropped from `priority`'s full queue.
    pub(crate) fn record_drop(&mut self, priority: OutboundPriority) {
        let slot = &mut self.dropped[priority.index()];
        *slot = slot.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_all_zero() {
        let metrics = PlayMetrics::new();
        assert_eq!(metrics.frames_decoded(), 0);
        assert_eq!(metrics.bytes_in(), 0);
        assert_eq!(metrics.over_budget(), 0);
        assert_eq!(metrics.frames_encoded(), 0);
        assert_eq!(metrics.bytes_out(), 0);
        assert_eq!(metrics.batches_flushed(), 0);
        assert_eq!(metrics.dropped_total(), 0);
    }

    #[test]
    fn records_accumulate() {
        let mut metrics = PlayMetrics::new();
        metrics.record_frame_decoded(10);
        metrics.record_frame_decoded(5);
        metrics.record_over_budget();
        metrics.record_frame_encoded(20);
        metrics.record_batch();
        metrics.record_drop(OutboundPriority::World);
        metrics.record_drop(OutboundPriority::World);
        metrics.record_drop(OutboundPriority::Cosmetic);

        assert_eq!(metrics.frames_decoded(), 2);
        assert_eq!(metrics.bytes_in(), 15);
        assert_eq!(metrics.over_budget(), 1);
        assert_eq!(metrics.frames_encoded(), 1);
        assert_eq!(metrics.bytes_out(), 20);
        assert_eq!(metrics.batches_flushed(), 1);
        assert_eq!(metrics.dropped(OutboundPriority::World), 2);
        assert_eq!(metrics.dropped(OutboundPriority::Cosmetic), 1);
        assert_eq!(metrics.dropped(OutboundPriority::Critical), 0);
        assert_eq!(metrics.dropped_total(), 3);
    }
}
