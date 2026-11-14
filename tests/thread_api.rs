#![deny(warnings, rust_2018_idioms)]
use loom::sync::mpsc::channel;
use loom::thread;

#[test]
fn initial_thread() {
    loom::model(|| {
        thread::current().id(); // can call id()
        assert_eq!(None, thread::current().name());
    });
}

#[test]
fn many_joins() {
    loom::model(|| {
        let mut handles = vec![];
        let mutex = loom::sync::Arc::new(loom::sync::Mutex::new(()));
        let lock = mutex.lock().unwrap();

        for _ in 1..3 {
            let mutex = mutex.clone();
            handles.push(thread::spawn(move || {
                mutex.lock().unwrap();
            }));
        }

        std::mem::drop(lock);

        for handle in handles.into_iter() {
            let _ = handle.join();
        }
    })
}

#[test]
fn is_finished() {
    loom::model(|| {
        let mut handles = vec![];
        let mutex = loom::sync::Arc::new(loom::sync::Mutex::new(()));
        let lock = mutex.lock().unwrap();

        for _ in 1..3 {
            let mutex = mutex.clone();
            handles.push(thread::spawn(move || {
                mutex.lock().unwrap();
            }));
        }

        for handle in &handles {
            assert!(!handle.is_finished());
        }

        std::mem::drop(lock);

        for handle in handles.into_iter() {
            while !handle.is_finished() {
                thread::yield_now();
            }
            let _ = handle.join();
        }
    })
}

#[test]
fn alt_join() {
    loom::model(|| {
        use loom::sync::{Arc, Mutex};

        let arcmut: Arc<Mutex<Option<thread::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
        let lock = arcmut.lock().unwrap();

        let arcmut2 = arcmut.clone();

        let th1 = thread::spawn(|| {});
        let th2 = thread::spawn(move || {
            arcmut2.lock().unwrap();
            let _ = th1.join();
        });
        let th3 = thread::spawn(move || {});
        std::mem::drop(lock);
        let _ = th3.join();
        let _ = th2.join();
    })
}

#[test]
fn threads_have_unique_ids() {
    loom::model(|| {
        let (tx, rx) = channel();
        let th1 = thread::spawn(move || tx.send(thread::current().id()));
        let thread_id_1 = rx.recv().unwrap();

        assert_eq!(th1.thread().id(), thread_id_1);
        assert_ne!(thread::current().id(), thread_id_1);
        let _ = th1.join();

        let (tx, rx) = channel();
        let th2 = thread::spawn(move || tx.send(thread::current().id()));
        let thread_id_2 = rx.recv().unwrap();
        assert_eq!(th2.thread().id(), thread_id_2);
        assert_ne!(thread::current().id(), thread_id_2);
        assert_ne!(thread_id_1, thread_id_2);
        let _ = th2.join();
    })
}

#[test]
fn thread_names() {
    loom::model(|| {
        let (tx, rx) = channel();
        let th = thread::spawn(move || tx.send(thread::current().name().map(|s| s.to_string())));
        assert_eq!(None, rx.recv().unwrap());
        assert_eq!(None, th.thread().name());
        let _ = th.join();

        let (tx, rx) = channel();
        let th = thread::Builder::new()
            .spawn(move || tx.send(thread::current().name().map(|s| s.to_string())))
            .unwrap();
        assert_eq!(None, rx.recv().unwrap());
        assert_eq!(None, th.thread().name());
        let _ = th.join();

        let (tx, rx) = channel();
        let th = thread::Builder::new()
            .name("foobar".to_string())
            .spawn(move || tx.send(thread::current().name().map(|s| s.to_string())))
            .unwrap();
        assert_eq!(Some("foobar".to_string()), rx.recv().unwrap());
        assert_eq!(Some("foobar"), th.thread().name());

        let _ = th.join();
    })
}

#[test]
fn thread_stack_size() {
    const STACK_SIZE: usize = 1 << 16;
    let mut builder = loom::model::Builder::new();
    // The global stack size is intentionally set too small. The thread's stack
    // size will override it.
    builder.stack_size = Some(1024);
    builder.check(|| {
        let body = || {
            // Allocate a large array on the stack.
            std::hint::black_box(&mut [0usize; STACK_SIZE]);
        };
        thread::Builder::new()
            .stack_size(
                // Include space for function calls in addition to the array.
                2 * STACK_SIZE,
            )
            .spawn(body)
            .unwrap()
            .join()
            .unwrap()
    })
}

#[test]
fn global_stack_size() {
    const STACK_SIZE: usize = 1 << 16;
    let mut builder = loom::model::Builder::new();
    // Include space for function calls in addition to the array.
    builder.stack_size = Some(2 * STACK_SIZE);
    builder.check(|| {
        let body = || {
            // Allocate a large array on the stack.
            std::hint::black_box(&mut [0usize; STACK_SIZE]);
        };
        body(); // The main thread has a large stack size too.
        thread::spawn(body).join().unwrap();
    })
}

/// Stacks are kept from one execution to the next, and a thread must not be
/// given one that is smaller than what it asked for, whatever order the
/// threads are spawned in.
#[test]
fn stack_sizes_across_executions() {
    use loom::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::atomic::AtomicUsize as StdAtomicUsize;
    use std::sync::Arc;

    const STACK_SIZE: usize = 1 << 16;

    let executions = Arc::new(StdAtomicUsize::new(0));
    let count = executions.clone();

    let mut builder = loom::model::Builder::new();
    builder.stack_size = Some(1 << 13);
    builder.check(move || {
        let execution = count.fetch_add(1, SeqCst);
        let value = Arc::new(AtomicUsize::new(0));

        let large = |value: Arc<AtomicUsize>| {
            thread::Builder::new()
                .stack_size(2 * STACK_SIZE)
                .spawn(move || {
                    std::hint::black_box(&mut [0usize; STACK_SIZE]);
                    value.fetch_add(1, SeqCst);
                })
                .unwrap()
        };
        let small = |value: Arc<AtomicUsize>| {
            thread::spawn(move || {
                value.fetch_add(1, SeqCst);
            })
        };

        let handles = if execution % 2 == 0 {
            [large(value.clone()), small(value.clone())]
        } else {
            [small(value.clone()), large(value.clone())]
        };

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(value.load(SeqCst), 2);
    });

    assert!(executions.load(SeqCst) > 1);
}

#[test]
fn park_unpark_loom() {
    loom::model(|| {
        println!("unpark");
        thread::current().unpark();
        println!("park");
        thread::park();
        println!("it did not deadlock");
    });
}

// Unparking a thread that is (or may be) blocked
// inside `JoinHandle::join` must not abort the model. On std this pattern is
// legal and harmless — `join` is not `park`, so the unpark just makes the
// token available.
#[test]
fn unpark_during_join() {
    loom::model(|| {
        let main = thread::current();
        let handle = thread::spawn(move || main.unpark());
        handle.join().unwrap();
    });
}

// The token deposited while the target was joining must survive the join,
// exactly as std guarantees: the `park` after the join consumes it and
// returns immediately in every schedule, instead of blocking a thread no
// one is left to wake.
#[test]
fn park_token_survives_join() {
    loom::model(|| {
        let main = thread::current();
        let handle = thread::spawn(move || main.unpark());
        handle.join().unwrap();
        thread::park();
    });
}

#[test]
fn park_unpark_std() {
    println!("unpark");
    std::thread::current().unpark();
    println!("park");
    std::thread::park();
    println!("it did not deadlock");
}

/// `is_finished` must be a point the model explores around: across all
/// executions both outcomes have to be observed for a thread racing with the
/// check.
#[test]
fn is_finished_explores_both_outcomes() {
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    use std::sync::Arc;

    let saw_finished = Arc::new(AtomicBool::new(false));
    let saw_running = Arc::new(AtomicBool::new(false));

    {
        let saw_finished = saw_finished.clone();
        let saw_running = saw_running.clone();
        loom::model(move || {
            let handle = thread::spawn(|| {});

            if handle.is_finished() {
                saw_finished.store(true, SeqCst);
            } else {
                saw_running.store(true, SeqCst);
            }

            handle.join().unwrap();
        });
    }

    assert!(saw_running.load(SeqCst), "never observed a running thread");
    assert!(
        saw_finished.load(SeqCst),
        "never observed a finished thread"
    );
}

fn incrementer(a: &loom::sync::atomic::AtomicUsize) -> impl FnOnce() + '_ {
    move || {
        let _ = a.fetch_add(1, loom::sync::atomic::Ordering::Relaxed);
    }
}

#[test]
fn scoped_thread() {
    loom::model(|| {
        const SPAWN_COUNT: usize = 3;
        let a = loom::sync::atomic::AtomicUsize::new(0);
        thread::scope(|scope| {
            for _i in 0..SPAWN_COUNT {
                let _handle = scope.spawn(incrementer(&a));
            }
        });
        assert_eq!(a.load(loom::sync::atomic::Ordering::Relaxed), SPAWN_COUNT);
    })
}

#[test]
fn scoped_thread_builder() {
    loom::model(|| {
        const SPAWN_COUNT: usize = 3;
        let a = loom::sync::atomic::AtomicUsize::new(0);
        thread::scope(|scope| {
            for _i in 0..SPAWN_COUNT {
                thread::Builder::new()
                    .spawn_scoped(scope, incrementer(&a))
                    .unwrap();
            }
        });
        assert_eq!(a.load(loom::sync::atomic::Ordering::Relaxed), SPAWN_COUNT);
    })
}

#[test]
fn scoped_thread_join() {
    loom::model(|| {
        const JOIN_COUNT: usize = 2;
        let a = loom::sync::atomic::AtomicUsize::new(0);
        thread::scope(|scope| {
            let handles = [(); JOIN_COUNT].map(|()| scope.spawn(incrementer(&a)));

            // Spawn another thread that might increment `a` before the first
            // threads finish.
            let _other_handle = scope.spawn(incrementer(&a));

            for h in handles {
                h.join().unwrap()
            }
            let a = a.load(loom::sync::atomic::Ordering::Relaxed);
            assert!(a == JOIN_COUNT || a == JOIN_COUNT + 1);
        });
        assert_eq!(
            a.load(loom::sync::atomic::Ordering::Relaxed),
            JOIN_COUNT + 1
        );
    })
}

#[test]
fn multiple_scopes() {
    loom::model(|| {
        let a = loom::sync::atomic::AtomicUsize::new(0);

        thread::scope(|scope| {
            let _handle = scope.spawn(incrementer(&a));
        });
        assert_eq!(a.load(loom::sync::atomic::Ordering::Relaxed), 1);

        thread::scope(|scope| {
            let _handle = scope.spawn(incrementer(&a));
        });
        assert_eq!(a.load(loom::sync::atomic::Ordering::Relaxed), 2);
    })
}

#[test]
fn scoped_and_unscoped_threads() {
    loom::model(|| {
        let a = loom::sync::Arc::new(loom::sync::atomic::AtomicUsize::new(0));

        let unscoped_handle = thread::scope(|scope| {
            let _handle = scope.spawn(incrementer(&a));
            let a = a.clone();
            loom::thread::spawn(move || incrementer(&a)())
        });

        let v = a.load(loom::sync::atomic::Ordering::Relaxed);
        assert!(v == 1 || v == 2, "{}", v);

        unscoped_handle.join().unwrap();
        let v = a.load(loom::sync::atomic::Ordering::Relaxed);
        assert_eq!(v, 2);
    })
}

struct YieldAndIncrementOnDrop<'a>(&'a std::sync::atomic::AtomicUsize);

impl Drop for YieldAndIncrementOnDrop<'_> {
    fn drop(&mut self) {
        thread::yield_now();
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn scoped_thread_wait_until_finished() {
    loom::model(|| {
        let a = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r: &std::sync::atomic::AtomicUsize = &a;
        thread::scope(|s| {
            s.spawn(move || {
                r.fetch_add(2, std::sync::atomic::Ordering::SeqCst);
                YieldAndIncrementOnDrop(r)
            });
        });
        assert_eq!(a.load(std::sync::atomic::Ordering::SeqCst), 3);
    });
}

#[test]
fn scoped_thread_join_handle_forgotten() {
    loom::model(|| {
        let a = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r: &std::sync::atomic::AtomicUsize = &a;
        thread::scope(|s| {
            let handle = s.spawn(move || {
                r.fetch_add(2, std::sync::atomic::Ordering::SeqCst);
                YieldAndIncrementOnDrop(r)
            });
            std::mem::forget(handle)
        });
        // Expect only 2 since the spawned thread will complete but its result
        // will be leaked and so never dropped.
        assert_eq!(a.load(std::sync::atomic::Ordering::SeqCst), 2);
    });
}

/// The scope waits for the thread even when the handle was joined explicitly
/// and the thread's result borrows from the environment.
#[test]
fn scoped_thread_borrowing_result() {
    loom::model(|| {
        let text = String::from("hello");
        let len = thread::scope(|s| {
            let handle = s.spawn(|| text.as_str());
            handle.join().unwrap().len()
        });
        assert_eq!(len, text.len());
        drop(text);
    });
}

/// A scoped thread may spawn further threads into the same scope; the scope
/// waits for those as well.
#[test]
fn scoped_thread_nested_spawn() {
    loom::model(|| {
        let a = loom::sync::atomic::AtomicUsize::new(0);
        thread::scope(|scope| {
            scope.spawn(|| {
                scope.spawn(incrementer(&a));
                incrementer(&a)();
            });
        });
        assert_eq!(a.load(loom::sync::atomic::Ordering::Relaxed), 2);
    })
}

/// Scoped threads run concurrently with the scope's body: both orders of two
/// racing operations are explored.
#[test]
fn scoped_thread_is_concurrent() {
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    use std::sync::Arc;

    let saw_before = Arc::new(AtomicBool::new(false));
    let saw_after = Arc::new(AtomicBool::new(false));

    {
        let saw_before = saw_before.clone();
        let saw_after = saw_after.clone();
        loom::model(move || {
            let a = loom::sync::atomic::AtomicUsize::new(0);
            let seen = thread::scope(|scope| {
                let handle = scope.spawn(incrementer(&a));
                let seen = a.load(loom::sync::atomic::Ordering::SeqCst);
                assert!(handle.is_finished() || seen == 0 || seen == 1);
                seen
            });

            match seen {
                0 => saw_before.store(true, SeqCst),
                _ => saw_after.store(true, SeqCst),
            }
        });
    }

    assert!(saw_before.load(SeqCst));
    assert!(saw_after.load(SeqCst));
}

/// A panic in the scope's body is propagated out of `scope`.
#[test]
#[should_panic(expected = "scope body panicked")]
fn scope_propagates_panic() {
    loom::model(|| {
        let a = loom::sync::atomic::AtomicUsize::new(0);
        thread::scope(|scope| {
            scope.spawn(incrementer(&a));
            panic!("scope body panicked");
        })
    })
}
