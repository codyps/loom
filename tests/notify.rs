#![deny(warnings, rust_2018_idioms)]

use loom::sync::Arc;
use loom::sync::Notify;
use loom::thread;

use std::pin::pin;

#[test]
fn notify_one_wakes_waiter() {
    loom::model(|| {
        let notify = Arc::new(Notify::new());
        let notify2 = notify.clone();

        let th = thread::spawn(move || {
            notify2.wait();
        });

        notify.notify_one();
        th.join().unwrap();
    });
}

#[test]
fn notify_last_wakes_waiter() {
    loom::model(|| {
        let notify = Arc::new(Notify::new());
        let notify2 = notify.clone();

        let th = thread::spawn(move || {
            notify2.wait();
        });

        notify.notify_last();
        th.join().unwrap();
    });
}

#[test]
fn notify_waiters_with_fallback() {
    // notify_waiters() does not store a permit, so a subsequent notify_one()
    // is needed to ensure the waiter completes in all interleavings.
    loom::model(|| {
        let notify = Arc::new(Notify::new());
        let notify2 = notify.clone();

        let th = thread::spawn(move || {
            // The first wait may end early with the `notify_waiters`
            // notification, or spuriously; the permit is still there for the
            // second one in that case, and is consumed by one of the two.
            notify2.wait();
        });

        notify.notify_waiters();
        notify.notify_one();
        th.join().unwrap();
    });
}

#[cfg(feature = "futures")]
#[test]
fn notified_with_stored_permit() {
    loom::model(|| {
        let notify = Notify::new();

        notify.notify_one();

        loom::future::block_on(notify.notified());
    });
}

#[cfg(feature = "futures")]
#[test]
fn notified_across_threads() {
    loom::model(|| {
        let notify = Arc::new(Notify::new());
        let notify2 = notify.clone();

        let th = thread::spawn(move || {
            loom::future::block_on(notify2.notified());
        });

        notify.notify_one();
        th.join().unwrap();
    });
}

#[cfg(feature = "futures")]
#[test]
fn notify_one_before_notified_completes_immediately() {
    loom::model(|| {
        let notify = Notify::new();

        notify.notify_one();
        notify.notify_one();

        loom::future::block_on(notify.notified());
    });
}

/// The original API: `notify` stores a permit that `wait` consumes.
#[test]
fn notify_then_wait() {
    loom::model(|| {
        let notify = Notify::new();

        notify.notify();
        notify.wait();
    });
}

/// At most one permit is stored.
#[test]
fn single_permit() {
    loom::model(|| {
        let notify = Notify::new();

        notify.notify_one();
        notify.notify_one();

        let mut first = pin!(notify.notified());
        let mut second = pin!(notify.notified());

        assert!(first.as_mut().enable());
        assert!(!second.as_mut().enable());
    });
}

/// `notify_waiters` wakes what is waiting and leaves nothing behind.
#[test]
fn notify_waiters_stores_no_permit() {
    loom::model(|| {
        let notify = Notify::new();

        notify.notify_waiters();

        let mut later = pin!(notify.notified());
        assert!(!later.as_mut().enable());
    });
}

/// A future receives `notify_waiters` notifications from the moment it is
/// created, even if it has not been polled.
#[test]
fn notify_waiters_reaches_unpolled_futures() {
    loom::model(|| {
        let notify = Notify::new();

        let mut first = pin!(notify.notified());
        let mut second = pin!(notify.notified());

        notify.notify_waiters();

        assert!(first.as_mut().enable());
        assert!(second.as_mut().enable());
    });
}

/// `notify_one` wakes the oldest waiter, `notify_last` the newest.
#[test]
fn notify_one_and_last_ordering() {
    loom::model(|| {
        let notify = Notify::new();

        let mut first = pin!(notify.notified());
        let mut second = pin!(notify.notified());
        let mut third = pin!(notify.notified());

        assert!(!first.as_mut().enable());
        assert!(!second.as_mut().enable());
        assert!(!third.as_mut().enable());

        notify.notify_one();
        assert!(first.as_mut().enable());
        assert!(!second.as_mut().enable());
        assert!(!third.as_mut().enable());

        notify.notify_last();
        assert!(!second.as_mut().enable());
        assert!(third.as_mut().enable());

        // No permit was stored while there were waiters.
        let mut fourth = pin!(notify.notified());
        assert!(!fourth.as_mut().enable());
    });
}

/// A `notify_one` notification that is never observed, because the notified
/// future is dropped, is passed on to the next waiter.
#[test]
fn dropped_notification_is_forwarded() {
    loom::model(|| {
        let notify = Notify::new();

        let mut second = pin!(notify.notified());

        {
            let mut first = pin!(notify.notified());
            assert!(!first.as_mut().enable());
            assert!(!second.as_mut().enable());

            notify.notify_one();
        }

        assert!(second.as_mut().enable());
    });
}

/// A dropped waiter that was not notified simply stops waiting.
#[test]
fn dropped_waiter_is_removed() {
    loom::model(|| {
        let notify = Notify::new();

        {
            let mut first = pin!(notify.notified());
            assert!(!first.as_mut().enable());
        }

        // With no waiter left, this becomes a permit.
        notify.notify_one();

        let mut second = pin!(notify.notified());
        assert!(second.as_mut().enable());
    });
}

/// A thread blocked in `wait` and a task awaiting `notified` are both woken
/// by `notify_waiters`, once they are both waiting.
#[cfg(feature = "futures")]
#[test]
fn notify_waiters_wakes_everything() {
    use loom::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    loom::model(|| {
        let state = Arc::new((Notify::new(), AtomicUsize::new(0)));

        let th = {
            let state = state.clone();
            thread::spawn(move || {
                let notified = state.0.notified();
                state.1.fetch_add(1, SeqCst);
                loom::future::block_on(notified);
            })
        };

        // Created before `notify_waiters` is called.
        let notified = state.0.notified();

        // Wait for the other future to exist as well.
        while state.1.load(SeqCst) == 0 {
            thread::yield_now();
        }

        state.0.notify_waiters();

        loom::future::block_on(notified);
        th.join().unwrap();
    });
}

/// Two tasks, two `notify_one` calls: both complete in every interleaving,
/// whether each notification finds a waiter or is stored as the permit.
#[cfg(feature = "futures")]
#[test]
fn notify_one_per_waiter() {
    loom::model(|| {
        let notify = Arc::new(Notify::new());

        let ths: Vec<_> = (0..2)
            .map(|_| {
                let notify = notify.clone();
                thread::spawn(move || {
                    loom::future::block_on(notify.notified());
                    notify.notify_one();
                })
            })
            .collect();

        notify.notify_one();

        for th in ths {
            th.join().unwrap();
        }

        // The last waiter's `notify_one` is left over as the permit.
        loom::future::block_on(notify.notified());
    });
}

/// Without a notification, a waiter never completes: loom reports the
/// deadlock rather than letting the wait through.
#[cfg(feature = "futures")]
#[test]
#[should_panic(expected = "deadlock")]
fn notified_without_notification_deadlocks() {
    loom::model(|| {
        let notify = Notify::new();

        loom::future::block_on(notify.notified());
    });
}

/// Several threads can be blocked in `wait` at once: `notify_one` wakes one
/// of two, and the other keeps waiting until it is notified as well.
#[test]
fn notify_one_wakes_one_of_several() {
    use loom::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering::{Acquire, Release};

    loom::model(|| {
        let notify = Arc::new(Notify::new());
        let woken = Arc::new(AtomicUsize::new(0));

        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let (notify, woken) = (notify.clone(), woken.clone());
                thread::spawn(move || {
                    notify.wait();
                    woken.fetch_add(1, Release);
                })
            })
            .collect();

        // One permit is stored at most: each `notify_one` is only given
        // once the one before has been taken.
        notify.notify_one();
        while woken.load(Acquire) < 1 {
            thread::yield_now();
        }
        notify.notify_one();

        for waiter in waiters {
            waiter.join().unwrap();
        }
    });
}
