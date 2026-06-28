//! A bounded, per-source-IP concurrent-connection limiter for the accept loop.
//!
//! [`PerIpConnections`] tracks how many connections are currently live from each
//! source IP and refuses a new one once that IP reaches a configured cap. It is
//! the per-IP counterpart to the global connection [`Semaphore`] the acceptor
//! already holds: the semaphore bounds *total* concurrency, this bounds *per-IP*
//! concurrency so a single host cannot monopolise the connection budget.
//!
//! ## What is bounded
//!
//! The internal map holds **at most one entry per currently-connected source IP**.
//! An entry is created on the first live connection from an IP and removed the
//! instant its last connection drops, so the map can never accumulate stale
//! entries. Because every counted connection also holds a global semaphore permit,
//! the number of map entries is itself bounded by the server's global
//! `max_connections` ceiling — it cannot grow without bound under a flood of
//! distinct source IPs.
//!
//! ## Backpressure
//!
//! There is no queueing. [`PerIpConnections::try_acquire`] is non-blocking: when
//! the calling IP is already at its limit it returns `None` and the acceptor drops
//! the new connection immediately. A successful acquire returns an
//! [`IpConnectionGuard`] whose `Drop` releases the slot (and prunes the entry when
//! the count reaches zero).
//!
//! [`Semaphore`]: tokio::sync::Semaphore

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};

/// Tracks live connection counts per source IP and enforces a per-IP cap.
///
/// Construct with [`PerIpConnections::new`], wrap in an [`Arc`], and call
/// [`try_acquire`](Self::try_acquire) once per accepted connection. Hold the
/// returned [`IpConnectionGuard`] for the connection's lifetime.
#[derive(Debug)]
pub struct PerIpConnections {
    /// Maximum concurrent connections allowed per IP; `0` disables the limit.
    limit: usize,
    /// Live connection count per source IP. Entries are pruned at zero, so the
    /// map only ever holds currently-connected IPs. A plain `std::sync::Mutex`
    /// guards a tiny critical section (one map insert/decrement) and is never held
    /// across an `.await`.
    counts: Mutex<HashMap<IpAddr, usize>>,
}

impl PerIpConnections {
    /// Builds a limiter that allows at most `limit` concurrent connections per IP.
    ///
    /// A `limit` of `0` disables per-IP limiting entirely: every acquire succeeds
    /// and no per-IP state is tracked.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            counts: Mutex::new(HashMap::new()),
        }
    }

    /// Attempts to reserve a connection slot for `ip`.
    ///
    /// Returns `Some(guard)` if the IP is below its limit (incrementing its live
    /// count), or `None` if it is already at the limit. When the configured limit
    /// is `0` (disabled) this always returns `Some` and tracks no state. The
    /// returned guard releases the slot on drop.
    pub fn try_acquire(self: &Arc<Self>, ip: IpAddr) -> Option<IpConnectionGuard> {
        if self.limit == 0 {
            // Disabled: hand back a guard that tracks nothing and is a no-op on drop.
            return Some(IpConnectionGuard {
                limiter: Arc::clone(self),
                ip,
                tracked: false,
            });
        }
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = counts.entry(ip).or_insert(0);
        if *entry >= self.limit {
            return None;
        }
        *entry += 1;
        Some(IpConnectionGuard {
            limiter: Arc::clone(self),
            ip,
            tracked: true,
        })
    }

    /// Decrements the live count for `ip`, pruning the entry when it reaches zero.
    /// Called only from [`IpConnectionGuard::drop`].
    fn release(&self, ip: IpAddr) {
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = counts.get_mut(&ip) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&ip);
            }
        }
    }

    /// The number of source IPs with at least one live connection.
    ///
    /// Exposed for observability and tests; it is exactly the number of entries
    /// in the bounded map.
    #[must_use]
    pub fn tracked_ips(&self) -> usize {
        self.counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// An RAII reservation of one per-IP connection slot.
///
/// Held for the lifetime of an accepted connection. Dropping it releases the slot
/// back to the [`PerIpConnections`] limiter (and prunes the IP's map entry when
/// it was the last live connection from that IP).
#[derive(Debug)]
pub struct IpConnectionGuard {
    limiter: Arc<PerIpConnections>,
    ip: IpAddr,
    /// `false` when the limiter is disabled (`limit == 0`): drop does nothing.
    tracked: bool,
}

impl Drop for IpConnectionGuard {
    fn drop(&mut self) {
        if self.tracked {
            self.limiter.release(self.ip);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, last))
    }

    #[test]
    fn acquires_up_to_the_limit_then_refuses() {
        let limiter = Arc::new(PerIpConnections::new(3));
        let addr = ip(1);
        // Three from the same IP succeed; the fourth is refused.
        let mut held: Vec<IpConnectionGuard> = (0..3)
            .map(|_| limiter.try_acquire(addr).expect("under the limit"))
            .collect();
        assert!(
            limiter.try_acquire(addr).is_none(),
            "fourth connection from the same IP must be refused"
        );
        assert_eq!(limiter.tracked_ips(), 1);

        // Releasing one frees exactly one slot.
        held.pop();
        let regained = limiter.try_acquire(addr);
        assert!(regained.is_some(), "a freed slot is reusable");
    }

    #[test]
    fn distinct_ips_are_independent() {
        let limiter = Arc::new(PerIpConnections::new(1));
        let a = limiter.try_acquire(ip(1)).expect("first IP");
        let b = limiter.try_acquire(ip(2)).expect("second IP");
        // Each IP gets its own budget.
        assert!(limiter.try_acquire(ip(1)).is_none());
        assert!(limiter.try_acquire(ip(2)).is_none());
        assert_eq!(limiter.tracked_ips(), 2);
        drop((a, b));
    }

    #[test]
    fn entry_is_pruned_when_last_connection_drops() {
        let limiter = Arc::new(PerIpConnections::new(2));
        let g1 = limiter.try_acquire(ip(1)).expect("first");
        let g2 = limiter.try_acquire(ip(1)).expect("second");
        assert_eq!(limiter.tracked_ips(), 1);
        drop(g1);
        assert_eq!(limiter.tracked_ips(), 1, "still one live connection");
        drop(g2);
        assert_eq!(limiter.tracked_ips(), 0, "map entry pruned at zero");
    }

    #[test]
    fn zero_limit_disables_tracking() {
        let limiter = Arc::new(PerIpConnections::new(0));
        let guards: Vec<IpConnectionGuard> = (0..100)
            .map(|_| limiter.try_acquire(ip(1)).expect("unlimited"))
            .collect();
        // Disabled => no per-IP state is tracked even with many live connections.
        assert_eq!(limiter.tracked_ips(), 0);
        drop(guards);
    }

    #[test]
    fn simulated_burst_from_one_ip_admits_exactly_the_limit() {
        let limit = 5;
        let limiter = Arc::new(PerIpConnections::new(limit));
        let addr = ip(42);
        // Simulate N > limit connection attempts from the same IP.
        let attempts = 20;
        let admitted: Vec<IpConnectionGuard> = (0..attempts)
            .filter_map(|_| limiter.try_acquire(addr))
            .collect();
        assert_eq!(
            admitted.len(),
            limit,
            "only `limit` connections are admitted"
        );
    }
}
