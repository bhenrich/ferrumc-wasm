//! A fixed-capacity ring buffer used to retain the most recent packet traces.

/// A fixed-capacity ring buffer that overwrites the oldest element once full.
///
/// The capacity `N` is a const generic, so the buffer never allocates and never
/// grows past `N` elements: a [`push`](Self::push) into a full buffer evicts the
/// oldest element in O(1). The backing storage is `[Option<T>; N]`, built with
/// [`std::array::from_fn`] (no `unsafe`, satisfying the crate's
/// `forbid(unsafe_code)`), so `T` needs no `Default` or `Copy` bound.
///
/// A zero-capacity buffer (`N == 0`) is legal: [`push`](Self::push) is a no-op,
/// [`len`](Self::len) stays `0`, and iteration yields nothing.
#[derive(Debug, Clone)]
pub struct RingBuffer<T, const N: usize> {
    /// Backing slots; only the `len` elements reachable from `head` are live.
    slots: [Option<T>; N],
    /// Index the next [`push`](Self::push) writes to (wraps at `N`).
    head: usize,
    /// Number of live elements, always in `0..=N`.
    len: usize,
}

impl<T, const N: usize> RingBuffer<T, N> {
    /// Creates an empty ring buffer of capacity `N`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            head: 0,
            len: 0,
        }
    }

    /// The fixed capacity of this buffer (the const generic `N`).
    #[must_use]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// The number of live elements currently held (always `<= N`).
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer holds no elements.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the buffer is at capacity (the next push evicts the oldest).
    ///
    /// A zero-capacity buffer is always considered full.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.len == N
    }

    /// Appends `value`, overwriting the oldest element when the buffer is full.
    ///
    /// Runs in O(1) and never allocates. A no-op when `N == 0`.
    pub fn push(&mut self, value: T) {
        if N == 0 {
            return;
        }
        self.slots[self.head] = Some(value);
        self.head = (self.head + 1) % N;
        if self.len < N {
            self.len += 1;
        }
    }

    /// Iterates the live elements from oldest to newest.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        // When not yet full the oldest element sits at index 0; once full it sits
        // at `head` (the slot the next push would overwrite).
        let start = if self.len < N { 0 } else { self.head };
        (0..self.len).filter_map(move |offset| self.slots[(start + offset) % N].as_ref())
    }

    /// Collects the live elements into a `Vec`, oldest to newest.
    #[must_use]
    pub fn to_vec(&self) -> Vec<T>
    where
        T: Clone,
    {
        self.iter().cloned().collect()
    }
}

impl<T, const N: usize> Default for RingBuffer<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_reports_empty() {
        let ring = RingBuffer::<u32, 4>::new();
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.capacity(), 4);
        assert!(ring.is_empty());
        assert!(!ring.is_full());
        assert_eq!(ring.to_vec(), Vec::<u32>::new());
    }

    #[test]
    fn fills_in_order_until_full() {
        let mut ring = RingBuffer::<u32, 3>::new();
        ring.push(1);
        ring.push(2);
        assert_eq!(ring.to_vec(), vec![1, 2]);
        assert!(!ring.is_full());
        ring.push(3);
        assert!(ring.is_full());
        assert_eq!(ring.to_vec(), vec![1, 2, 3]);
    }

    #[test]
    fn overwrites_oldest_and_keeps_order_on_wrap() {
        let mut ring = RingBuffer::<u32, 3>::new();
        for value in 1..=5 {
            ring.push(value);
        }
        // Capacity is 3, so only the three newest survive, oldest -> newest.
        assert_eq!(ring.len(), 3);
        assert!(ring.is_full());
        assert_eq!(ring.to_vec(), vec![3, 4, 5]);
    }

    #[test]
    fn never_grows_past_capacity() {
        let mut ring = RingBuffer::<u32, 8>::new();
        for value in 0..1_000 {
            ring.push(value);
        }
        assert_eq!(ring.len(), 8);
        assert_eq!(ring.capacity(), 8);
        assert_eq!(ring.to_vec(), vec![992, 993, 994, 995, 996, 997, 998, 999]);
    }

    #[test]
    fn capacity_one_keeps_only_newest() {
        let mut ring = RingBuffer::<u32, 1>::new();
        ring.push(10);
        assert_eq!(ring.to_vec(), vec![10]);
        ring.push(20);
        assert_eq!(ring.to_vec(), vec![20]);
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn capacity_zero_is_a_total_no_op() {
        let mut ring = RingBuffer::<u32, 0>::new();
        assert!(ring.is_empty());
        // `N == 0` is always "full" (capacity reached) and push does nothing.
        assert!(ring.is_full());
        ring.push(1);
        ring.push(2);
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.to_vec(), Vec::<u32>::new());
        assert_eq!(ring.iter().count(), 0);
    }

    #[test]
    fn iter_matches_to_vec_after_partial_wrap() {
        let mut ring = RingBuffer::<u32, 4>::new();
        for value in 1..=6 {
            ring.push(value);
        }
        let collected: Vec<u32> = ring.iter().copied().collect();
        assert_eq!(collected, vec![3, 4, 5, 6]);
        assert_eq!(collected, ring.to_vec());
    }
}
