//! The shared, bounded, graceful TCP accept loop.
//!
//! Both the status server (M09) and the login server (M11) run the same
//! connection-per-task acceptor: a [`tokio::sync::Semaphore`] caps concurrency,
//! a `watch` channel asks live connections to wind down, and a [`JoinSet`]
//! tracks the spawned tasks so shutdown can drain them. Only the per-connection
//! handler differs, so the loop itself lives here once.

use std::future::Future;
use std::io;
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;

/// Runs the accept loop on `listener` until `shutdown` resolves, then drains
/// in-flight connections.
///
/// `handle_conn` is invoked once per accepted socket with the [`TcpStream`] and a
/// wind-down [`watch::Receiver`] that flips to `true` during graceful shutdown;
/// the returned future is spawned as the connection task. Each task holds a
/// semaphore permit for its lifetime, so at most `max_connections` run at once.
///
/// ## Backpressure
///
/// A permit is reserved *before* `accept` is called, so when `max_connections`
/// connections are in flight the acceptor stops accepting and further peers wait
/// in the kernel's accept backlog (and are refused by the OS once it fills). The
/// server never spawns an unbounded number of tasks.
///
/// ## Shutdown
///
/// When `shutdown` resolves the loop stops accepting, signals every live
/// connection to wind down, and waits for the connection tasks to finish before
/// returning. A single failed `accept` (a peer reset, a transient resource
/// hiccup) is skipped rather than taking the acceptor down; only the semaphore
/// closing — which never happens while the server runs — ends the loop early.
pub(crate) async fn run<S, F, Fut>(
    listener: TcpListener,
    max_connections: usize,
    shutdown: S,
    handle_conn: F,
) -> io::Result<()>
where
    S: Future<Output = ()> + Send,
    F: Fn(TcpStream, watch::Receiver<bool>) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let slots = Arc::new(Semaphore::new(max_connections));
    // `false` = running. Flipped to `true` once to ask live connections to close
    // on the next read boundary during graceful shutdown.
    let (winddown_tx, winddown_rx) = watch::channel(false);
    let mut tasks: JoinSet<()> = JoinSet::new();

    tokio::pin!(shutdown);
    loop {
        // Reap completed connection tasks so the set never accumulates finished
        // handles during a connection storm.
        while tasks.try_join_next().is_some() {}

        // Reserve a connection slot *before* accepting: this bounds the number of
        // concurrent connections and lets the OS backlog absorb the overflow.
        let permit = tokio::select! {
            () = &mut shutdown => break,
            slot = Arc::clone(&slots).acquire_owned() => match slot {
                Ok(permit) => permit,
                // The semaphore is never closed while the server runs.
                Err(_) => break,
            },
        };

        let (stream, _peer) = tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                // A single failed accept (e.g. the peer reset before we accepted,
                // or a transient resource hiccup) must not take the acceptor down.
                Err(_) => continue,
            },
        };

        let task = handle_conn(stream, winddown_rx.clone());
        tasks.spawn(async move {
            // The permit lives for the connection's lifetime and frees the slot
            // when the task ends.
            let _permit = permit;
            task.await;
        });
    }

    // Ask live connections to close, then wait for every task to finish.
    let _ = winddown_tx.send(true);
    while tasks.join_next().await.is_some() {}
    Ok(())
}
