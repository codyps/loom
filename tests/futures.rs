#![cfg(feature = "futures")]
#![deny(warnings, rust_2018_idioms)]

use loom::future::{block_on, AtomicWaker};
use loom::sync::atomic::AtomicUsize;
use loom::thread;

use futures_util::future::poll_fn;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::task::Poll;

struct Chan {
    num: AtomicUsize,
    task: AtomicWaker,
}

#[test]
fn atomic_waker_valid() {
    use std::task::Poll::*;

    const NUM_NOTIFY: usize = 2;

    loom::model(|| {
        let chan = Arc::new(Chan {
            num: AtomicUsize::new(0),
            task: AtomicWaker::new(),
        });

        for _ in 0..NUM_NOTIFY {
            let chan = chan.clone();

            thread::spawn(move || {
                chan.num.fetch_add(1, Relaxed);
                chan.task.wake();
            });
        }

        block_on(poll_fn(move |cx| {
            chan.task.register_by_ref(cx.waker());

            if NUM_NOTIFY == chan.num.load(Relaxed) {
                return Ready(());
            }

            Pending
        }));
    });
}

// Tests futures spuriously poll as this is a very common pattern
#[test]
fn spurious_poll() {
    use loom::sync::atomic::AtomicBool;
    use loom::sync::atomic::Ordering::{Acquire, Release};

    let poll_thrice = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let actual = poll_thrice.clone();

    loom::model(move || {
        let gate = Arc::new(AtomicBool::new(false));
        let mut cnt = 0;

        let num_poll = block_on(poll_fn(|cx| {
            if cnt == 0 {
                let gate = gate.clone();
                let waker = cx.waker().clone();

                thread::spawn(move || {
                    gate.store(true, Release);
                    waker.wake();
                });
            }

            cnt += 1;

            if gate.load(Acquire) {
                Poll::Ready(cnt)
            } else {
                Poll::Pending
            }
        }));

        if num_poll == 3 {
            poll_thrice.store(true, Release);
        }

        assert!(num_poll > 0 && num_poll <= 3, "actual = {}", num_poll);
    });

    assert!(actual.load(Acquire));
}

/// A waker cloned from `block_on`'s context must compare equal (per
/// `Waker::will_wake`) to the waker handed to subsequent polls. Futures rely
/// on this to avoid re-registering, and waking themselves, on every poll.
#[test]
fn block_on_waker_will_wake_clone() {
    loom::model(|| {
        let mut saved: Option<std::task::Waker> = None;

        block_on(poll_fn(move |cx| match saved.take() {
            None => {
                let waker = cx.waker().clone();
                assert!(cx.waker().will_wake(&waker));
                assert!(waker.will_wake(&waker.clone()));
                waker.wake_by_ref();
                saved = Some(waker);
                Poll::Pending
            }
            Some(waker) => {
                assert!(cx.waker().will_wake(&waker));
                assert!(waker.will_wake(cx.waker()));
                Poll::Ready(())
            }
        }));
    });
}
