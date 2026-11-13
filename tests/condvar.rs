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

/// A `wait` that returns because of a stale unpark token (a spurious wakeup
/// as far as the condvar is concerned) must not leave the thread queued as a
/// waiter: the next `notify_one` has to reach a thread that is really
/// waiting.
#[test]
fn stale_unpark_token_does_not_steal_notification() {
    use loom::sync::atomic::AtomicBool;

    loom::model(|| {
        let state = Arc::new((Mutex::new(()), Condvar::new(), AtomicBool::new(false)));

        // Bank an unpark token for the main thread.
        thread::current().unpark();

        let waiter = {
            let state = state.clone();
            thread::spawn(move || {
                let (mutex, condvar, ready) = &*state;
                let mut guard = mutex.lock().unwrap();
                while !ready.load(SeqCst) {
                    guard = condvar.wait(guard).unwrap();
                }
            })
        };

        let (mutex, condvar, ready) = &*state;

        // Returns immediately by consuming the token.
        drop(condvar.wait(mutex.lock().unwrap()).unwrap());

        {
            let _guard = mutex.lock().unwrap();
            ready.store(true, SeqCst);
        }
        condvar.notify_one();

        waiter.join().unwrap();
    });
}
