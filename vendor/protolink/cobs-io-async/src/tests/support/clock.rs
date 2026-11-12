use core::task::{Context, Poll, Waker};
use std::{cell::RefCell, rc::Rc, vec::Vec};

use crate::Timer;

#[derive(Default)]
struct State {
    now: u64,
    waiters: Vec<(u64, Waker)>,
    latest_only: bool,
}

#[derive(Clone, Default)]
pub(in crate::tests) struct FakeClock(Rc<RefCell<State>>);

impl FakeClock {
    /// A clock that retains only the waker of the latest `poll_deadline`.
    pub(in crate::tests) fn latest_waker_only() -> Self {
        let clock = Self::default();
        clock.0.borrow_mut().latest_only = true;
        clock
    }

    pub(in crate::tests) fn advance(&self, ticks: u64) {
        let mut state = self.0.borrow_mut();
        state.now += ticks;
        let now = state.now;
        let (due, waiting): (Vec<_>, Vec<_>) = state
            .waiters
            .drain(..)
            .partition(|(deadline, _)| *deadline <= now);
        state.waiters = waiting;
        drop(state);
        for (_, waker) in due {
            waker.wake();
        }
    }

    pub(in crate::tests) fn deadlines(&self) -> Vec<u64> {
        let mut deadlines: Vec<u64> = self.0.borrow().waiters.iter().map(|(d, _)| *d).collect();
        deadlines.sort_unstable();
        deadlines.dedup();
        deadlines
    }
}

impl Timer for FakeClock {
    fn now(&self) -> u64 {
        self.0.borrow().now
    }

    fn poll_deadline(&mut self, cx: &mut Context<'_>, deadline: u64) -> Poll<()> {
        let mut state = self.0.borrow_mut();
        if state.now >= deadline {
            return Poll::Ready(());
        }
        if state.latest_only {
            state.waiters.clear();
        }
        state
            .waiters
            .retain(|(d, w)| !(*d == deadline && w.will_wake(cx.waker())));
        state.waiters.push((deadline, cx.waker().clone()));
        Poll::Pending
    }
}
