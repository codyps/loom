#![deny(warnings, rust_2018_idioms)]

use loom::sync::atomic::AtomicUsize;
use loom::sync::{Condvar, Mutex};
use loom::thread;

use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;

#[test]
fn notify_one() {
    loom::model(|| {
        let inc = Arc::new(Inc::new());

        for _ in 0..1 {
            let inc = inc.clone();
            thread::spawn(move || inc.inc());
        }

        inc.wait();
    });
}

#[test]
fn notify_all() {
    loom::model(|| {
        let inc = Arc::new(Inc::new());

        let mut waiters = Vec::new();
        for _ in 0..2 {
            let inc = inc.clone();
            waiters.push(thread::spawn(move || inc.wait()));
        }

        thread::spawn(move || inc.inc_all()).join().expect("inc");

        for th in waiters {
            th.join().expect("waiter");
        }
    });
}

#[test]
fn wait_while() {
    loom::model(|| {
        let inc = Arc::new(Inc::new());

        let j = {
            let inc = inc.clone();
            thread::spawn(move || inc.wait_for_1())
        };

        thread::spawn(move || inc.inc()).join().expect("inc");

        j.join().expect("waiter")
    });
}

struct Inc {
    num: AtomicUsize,
    mutex: Mutex<()>,
    condvar: Condvar,
}

impl Inc {
    fn new() -> Inc {
        Inc {
            num: AtomicUsize::new(0),
            mutex: Mutex::new(()),
            condvar: Condvar::new(),
        }
    }

    fn wait(&self) {
        let mut guard = self.mutex.lock().unwrap();

        loop {
            let val = self.num.load(SeqCst);
            if 1 == val {
                break;
            }

            guard = self.condvar.wait(guard).unwrap();
        }
    }

    fn wait_for_1(&self) {
        let guard = self.mutex.lock().unwrap();

        drop(
            self.condvar
                .wait_while(guard, |_| self.num.load(SeqCst) < 1)
                .unwrap(),
        );
    }

    fn inc(&self) {
        self.num.store(1, SeqCst);
        drop(self.mutex.lock().unwrap());
        self.condvar.notify_one();
    }

    fn inc_all(&self) {
        self.num.store(1, SeqCst);
        drop(self.mutex.lock().unwrap());
        self.condvar.notify_all();
    }
}

/// A condvar and `thread::park` are separate mechanisms: waiting on the
/// condvar does not use up an unpark token, which is still there for the
/// `park` that follows.
#[test]
fn condvar_wait_keeps_unpark_token() {
    loom::model(|| {
        let cv = Arc::new((Mutex::new(false), Condvar::new()));
        let main = thread::current();

        let th = {
            let cv = cv.clone();
            thread::spawn(move || {
                main.unpark();
                *cv.0.lock().unwrap() = true;
                cv.1.notify_one();
            })
        };

        // Step 1: wait on the mutex / condvar.
        let mut guard = cv.0.lock().unwrap();
        while !*guard {
            guard = cv.1.wait(guard).unwrap();
        }
        drop(guard);

        // Step 2: wait on `park()`.
        thread::park();

        th.join().unwrap();
    });
}

/// The other direction: `unpark` is not a condvar notification. A waiter
/// that is only ever unparked stays blocked, which is a deadlock.
#[test]
#[should_panic(expected = "deadlock")]
fn unpark_does_not_wake_condvar_waiter() {
    loom::model(|| {
        let cv = Arc::new((Mutex::new(()), Condvar::new()));
        let main = thread::current();

        let th = thread::spawn(move || main.unpark());

        let guard = cv.0.lock().unwrap();
        drop(cv.1.wait(guard).unwrap());

        th.join().unwrap();
    });
}

/// ... and a notification is not an unpark token: with nobody waiting on the
/// condvar it is lost, so the `park` blocks forever.
#[test]
#[should_panic(expected = "deadlock")]
fn notify_does_not_unpark() {
    loom::model(|| {
        let cv = Arc::new(Condvar::new());

        let th = {
            let cv = cv.clone();
            thread::spawn(move || cv.notify_one())
        };

        th.join().unwrap();
        thread::park();
    });
}

/// A thread that has an unpark token banked and then waits on a condvar is
/// an ordinary waiter: it is woken by `notify_one`, and does not take a
/// notification away from another waiter.
#[test]
fn waiter_with_unpark_token() {
    use loom::sync::atomic::AtomicBool;

    loom::model(|| {
        let state = Arc::new((Mutex::new(()), Condvar::new(), AtomicBool::new(false)));

        // Bank an unpark token for the main thread.
        thread::current().unpark();

        let notifier = {
            let state = state.clone();
            thread::spawn(move || {
                let (mutex, condvar, ready) = &*state;
                {
                    let _guard = mutex.lock().unwrap();
                    ready.store(true, SeqCst);
                }
                condvar.notify_one();
            })
        };

        let (mutex, condvar, ready) = &*state;

        let mut guard = mutex.lock().unwrap();
        while !ready.load(SeqCst) {
            guard = condvar.wait(guard).unwrap();
        }
        drop(guard);

        notifier.join().unwrap();

        // The token is still there.
        thread::park();
    });
}

/// A flag behind a mutex, and a condition variable to wait for it with.
struct Flag {
    set: Mutex<bool>,
    changed: Condvar,
}

impl Flag {
    fn new() -> Arc<Flag> {
        Arc::new(Flag {
            set: Mutex::new(false),
            changed: Condvar::new(),
        })
    }

    fn set(&self) {
        *self.set.lock().unwrap() = true;
        self.changed.notify_one();
    }
}

/// Waits once, without checking again that the flag is set.
fn wait_once(flag: &Flag) -> bool {
    let set = flag.set.lock().unwrap();

    if *set {
        return true;
    }

    *flag.changed.wait(set).unwrap()
}

/// A wait may end without a notification, as with `std`. Code that does not
/// check again, in a loop, is caught.
#[test]
#[should_panic(expected = "woke up without the flag")]
fn spurious_wakeup_is_explored() {
    let mut builder = loom::model::Builder::new();
    builder.spurious_wakeups = true;

    builder.check(|| {
        let flag = Flag::new();

        let setter = {
            let flag = flag.clone();
            thread::spawn(move || flag.set())
        };

        assert!(wait_once(&flag), "woke up without the flag");

        setter.join().unwrap();
    });
}

/// With spurious wakeups turned off, a wait only ends with a notification.
#[test]
fn spurious_wakeups_can_be_turned_off() {
    let mut builder = loom::model::Builder::new();
    builder.spurious_wakeups = false;

    builder.check(|| {
        let flag = Flag::new();

        let setter = {
            let flag = flag.clone();
            thread::spawn(move || flag.set())
        };

        assert!(wait_once(&flag));

        setter.join().unwrap();
    });
}

/// What a timed wait for the flag sees: whether it timed out, and whether
/// the flag was set.
fn timed_waits(wait: fn(&Flag) -> (bool, bool)) -> std::collections::BTreeSet<(bool, bool)> {
    let seen = Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new()));
    let recorded = seen.clone();

    loom::model(move || {
        let flag = Flag::new();

        let setter = {
            let flag = flag.clone();
            thread::spawn(move || flag.set())
        };

        recorded.lock().unwrap().insert(wait(&flag));

        setter.join().unwrap();
    });

    let seen = seen.lock().unwrap().clone();
    seen
}

/// A timed wait can time out, whatever the duration, and can be notified.
/// A wait that is notified does not time out.
#[test]
fn wait_timeout_times_out() {
    let seen = timed_waits(|flag| {
        let set = flag.set.lock().unwrap();

        if *set {
            return (false, true);
        }

        let (set, result) = flag
            .changed
            .wait_timeout(set, std::time::Duration::from_secs(1))
            .unwrap();

        (result.timed_out(), *set)
    });

    // Timed out before the flag was set, or after; notified, with the flag
    // set; and the flag already set before the wait.
    assert_eq!(seen, [(true, false), (true, true), (false, true)].into(),);
}

/// `wait_timeout_while` reports a timeout only with the condition still
/// true, and otherwise returns once it is false.
#[test]
fn wait_timeout_while_times_out() {
    let seen = timed_waits(|flag| {
        let (set, result) = flag
            .changed
            .wait_timeout_while(
                flag.set.lock().unwrap(),
                std::time::Duration::from_secs(1),
                |set| !*set,
            )
            .unwrap();

        (result.timed_out(), *set)
    });

    assert_eq!(seen, [(true, false), (false, true)].into());
}

/// A loop that waits again after a timeout ends: a thread times out at most
/// once per condition variable in an execution.
#[test]
fn wait_timeout_in_a_loop() {
    loom::model(|| {
        let flag = Flag::new();

        let setter = {
            let flag = flag.clone();
            thread::spawn(move || flag.set())
        };

        let mut set = flag.set.lock().unwrap();
        let mut timeouts = 0;

        while !*set {
            let (next, result) = flag
                .changed
                .wait_timeout(set, std::time::Duration::from_millis(10))
                .unwrap();
            set = next;

            if result.timed_out() {
                timeouts += 1;
            }
        }

        assert!(timeouts <= 1);
        drop(set);

        setter.join().unwrap();
    });
}
