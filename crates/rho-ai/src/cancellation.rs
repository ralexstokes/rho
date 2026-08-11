use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
};

/// A runtime-independent cooperative cancellation signal.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    state: Arc<State>,
}

#[derive(Debug, Default)]
struct State {
    cancelled: AtomicBool,
    waiters: Mutex<Vec<Waker>>,
}

impl CancellationToken {
    /// Creates an uncancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation and wakes all current waiters.
    pub fn cancel(&self) {
        if self.state.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }

        let waiters = match self.state.waiters.lock() {
            Ok(mut waiters) => std::mem::take(&mut *waiters),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        for waiter in waiters {
            waiter.wake();
        }
    }

    /// Returns whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    /// Waits until cancellation is requested.
    #[must_use]
    pub fn cancelled(&self) -> Cancelled {
        Cancelled {
            token: self.clone(),
        }
    }
}

/// Future returned by [`CancellationToken::cancelled`].
#[derive(Debug)]
pub struct Cancelled {
    token: CancellationToken,
}

impl Future for Cancelled {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.token.is_cancelled() {
            return Poll::Ready(());
        }

        let mut waiters = match self.token.state.waiters.lock() {
            Ok(waiters) => waiters,
            Err(poisoned) => poisoned.into_inner(),
        };
        if self.token.is_cancelled() {
            return Poll::Ready(());
        }
        if !waiters
            .iter()
            .any(|waiter| waiter.will_wake(context.waker()))
        {
            waiters.push(context.waker().clone());
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::pin, task::Poll};

    use super::CancellationToken;

    #[test]
    fn cancellation_is_sticky_and_wakes_waiters() {
        let token = CancellationToken::new();
        let mut cancelled = pin!(token.cancelled());
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        assert!(matches!(
            cancelled.as_mut().poll(&mut context),
            Poll::Pending
        ));

        token.cancel();
        assert!(matches!(
            cancelled.as_mut().poll(&mut context),
            Poll::Ready(())
        ));
        assert!(token.is_cancelled());
    }
}
