//! A minimal, dependency-free `block_on` executor.
//!
//! The synthetic `sim` benchmark needs to drive exactly one `async` call during
//! its untimed setup: [`LoadedChunkMap::acquire`](ferrumc_sim::LoadedChunkMap).
//! Backed by an [`InMemoryStore`](ferrumc_storage::InMemoryStore) it resolves
//! inline (a storage miss, then a synchronous flat-world generation), so a tiny
//! park-based executor is enough and avoids pulling a full async runtime into the
//! benchmark harness.
//!
//! It uses only `std`: a [`Wake`] implementation that unparks the blocking
//! thread, and [`std::thread::park`] to sleep until woken. No `unsafe` is
//! required, so the crate's `#![forbid(unsafe_code)]` holds.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

/// A [`Wake`] that unparks a specific thread.
struct ThreadWaker(Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Drives `future` to completion on the current thread, parking between polls.
///
/// This is a blocking call intended for untimed benchmark setup only; it never
/// runs inside a measured region.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut context = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            // The waker unparks this thread when the future is ready to make
            // progress; until then, sleep instead of spinning.
            Poll::Pending => thread::park(),
        }
    }
}
