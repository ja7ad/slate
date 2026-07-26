//! Cooperative task yielding and synchronous busy-polling bridge.
//!
//! Soundness argument for [`block_on`]:
//! Every future this crate constructs is a state machine over
//! `AsyncFlash`/`AsyncMonotonicCounter` calls, and when those are
//! `BlockingFlash`/`BlockingCounter` wrappers the future NEVER returns
//! `Pending` for an I/O reason. The only `Pending` it can produce is a
//! `yield_now`, which self-wakes and is ready on the next poll. Therefore this
//! loop terminates in a bounded number of polls and never spins on a dead
//! waker. Safe Rust: `Waker::noop()` is stable since 1.85; no `unsafe`, so
//! `#![forbid(unsafe_code)]` is preserved.

use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

/// Cooperative yield: returns `Pending` exactly once, waking immediately.
/// Portable across Embassy, RTIC, lilos and `block_on` — it relies only on the
/// `Waker` contract, not on any executor API.
pub struct YieldNow(bool);

/// Yields once to the executor.
pub fn yield_now() -> YieldNow {
    YieldNow(false)
}

impl Future for YieldNow {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// Drives `fut` to completion by busy-polling with a no-op waker.
///
/// See module documentation for soundness and termination arguments.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = core::pin::pin!(fut);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => core::hint::spin_loop(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_yield_now_and_block_on() {
        let mut count = 0;
        let fut = async {
            count += 1;
            yield_now().await;
            count += 1;
            yield_now().await;
            count += 1;
        };
        block_on(fut);
        assert_eq!(count, 3);
    }
}
