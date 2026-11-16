#![deny(warnings, rust_2018_idioms)]

use loom::model::Builder;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::Arc;
use loom::thread;
use std::sync::{Arc as StdArc, Mutex};

#[test]
fn runs_one_execution() {
    let calls = StdArc::new(std::sync::atomic::AtomicUsize::new(0));
    let recorded = calls.clone();
    Builder::new().check_with_input(&[], move || {
        recorded.fetch_add(1, Ordering::Relaxed);
        let value = Arc::new(AtomicUsize::new(0));
        let writer = value.clone();
        let child = thread::spawn(move || writer.store(1, Ordering::Relaxed));
        value.load(Ordering::Relaxed);
        child.join().unwrap();
    });
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

fn trace(input: &[u8]) -> Vec<usize> {
    let output = StdArc::new(Mutex::new(Vec::new()));
    let recorded = output.clone();
    Builder::new().check_with_input(input, move || {
        let value = Arc::new(AtomicUsize::new(0));
        let writer = value.clone();
        let log = recorded.clone();
        let child = thread::spawn(move || {
            writer.store(1, Ordering::Relaxed);
            log.lock().unwrap().push(1);
            thread::yield_now();
            writer.store(2, Ordering::Relaxed);
            log.lock().unwrap().push(2);
        });
        let observed = value.load(Ordering::Relaxed);
        recorded.lock().unwrap().push(observed + 10);
        child.join().unwrap();
    });
    let result = output.lock().unwrap().clone();
    result
}

#[test]
fn input_replays_and_changes_execution() {
    let baseline = trace(&[]);
    assert_eq!(baseline, trace(&[]));
    let mut changed = false;
    for byte in 0..=255 {
        let input = [byte; 32];
        let first = trace(&input);
        assert_eq!(first, trace(&input));
        changed |= first != baseline;
    }
    assert!(changed, "input should affect the execution");
}

fn atomic_history(input: &[u8]) -> (usize, Vec<u8>) {
    let output = StdArc::new(Mutex::new((None, Vec::new())));
    let result = output.clone();
    Builder::new().check_with_input(input, move || {
        let value = Arc::new(AtomicUsize::new(0));
        let first = value.clone();
        let first_result = result.clone();
        let a = thread::spawn(move || {
            first.store(1, Ordering::Relaxed);
            first_result.lock().unwrap().1.push(b'a');
        });
        let second = value.clone();
        let second_result = result.clone();
        let b = thread::spawn(move || {
            second.store(2, Ordering::Relaxed);
            second_result.lock().unwrap().1.push(b'b');
        });
        let observed = value.load(Ordering::Relaxed);
        {
            let mut output = result.lock().unwrap();
            output.0 = Some(observed);
            output.1.push(b'r');
        }
        a.join().unwrap();
        b.join().unwrap();
    });
    let value = output.lock().unwrap();
    (value.0.unwrap(), value.1.clone())
}

#[test]
fn input_selects_different_atomic_read_histories() {
    let older_input = [1, 1, 0, 0, 0, 0, 0, 0];
    let newer_input = [1, 1, 0, 0, 1, 0, 0, 0];
    let older = atomic_history(&older_input);
    let newer = atomic_history(&newer_input);

    assert_eq!(older, (0, vec![b'a', b'r', b'b']));
    assert_eq!(newer, (1, vec![b'a', b'r', b'b']));
    assert_eq!(atomic_history(&older_input), older);
    assert_eq!(atomic_history(&newer_input), newer);
}

#[test]
fn runs_once_even_with_checkpoint_settings() {
    let calls = StdArc::new(std::sync::atomic::AtomicUsize::new(0));
    let recorded = calls.clone();
    let mut builder = Builder::new();
    builder.checkpoint_file = Some("unused-input-checkpoint.json".into());
    builder.checkpoint_interval = 0;
    builder.max_permutations = Some(0);
    builder.max_duration = Some(std::time::Duration::ZERO);
    builder.check_with_input(&[255], move || {
        recorded.fetch_add(1, Ordering::Relaxed);
    });
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
#[should_panic(expected = "model failure")]
fn propagates_panics() {
    Builder::new().check_with_input(&[], || panic!("model failure"));
}

#[test]
#[should_panic(expected = "maximum number of branches")]
fn enforces_branch_limit() {
    let mut builder = Builder::new();
    builder.max_branches = 10;
    builder.check_with_input(&[], || loop {
        thread::yield_now();
    });
}

#[test]
fn respects_zero_preemption_bound() {
    let mut builder = Builder::new();
    builder.preemption_bound = Some(0);
    builder.check_with_input(&[255; 32], || {
        let value = Arc::new(AtomicUsize::new(0));
        let writer = value.clone();
        let child = thread::spawn(move || {
            writer.store(1, Ordering::SeqCst);
            thread::yield_now();
        });
        assert_eq!(value.load(Ordering::SeqCst), 0);
        thread::yield_now();
        child.join().unwrap();
    });
}

#[test]
fn allows_more_than_255_unbounded_preemptions() {
    let mut builder = Builder::new();
    builder.max_branches = 10_000;
    let input = [1, 0].repeat(2048);
    let output = StdArc::new(Mutex::new(Vec::new()));
    let recorded = output.clone();
    builder.check_with_input(&input, move || {
        let value = Arc::new(AtomicUsize::new(0));
        let writer = value.clone();
        let log = recorded.clone();
        let child = thread::spawn(move || {
            for _ in 0..300 {
                writer.fetch_add(1, Ordering::Relaxed);
                log.lock().unwrap().push(1);
            }
        });
        for _ in 0..300 {
            value.fetch_add(1, Ordering::Relaxed);
            recorded.lock().unwrap().push(0);
        }
        child.join().unwrap();
    });
    let trace = output.lock().unwrap();
    let switches = trace.windows(2).filter(|pair| pair[0] != pair[1]).count();
    assert!(switches > 255, "only observed {switches} switches");
}

#[test]
fn accepts_a_closure_that_consumes_non_sync_state() {
    let state = std::cell::Cell::new(String::from("model state"));
    Builder::new().check_with_input(&[], move || {
        assert_eq!(state.into_inner(), "model state");
    });
}

#[test]
#[should_panic(expected = "leaked")]
fn detects_leaks() {
    Builder::new().check_with_input(&[], || {
        std::mem::forget(Arc::new(AtomicUsize::new(0)));
    });
}

/// Threads that wait for each other by yielding: three on a spin lock, and
/// a thread waiting for one that waits for a third. No input starves one of
/// them, as a thread that yields is not chosen again before the others have
/// had a turn, and neither do the first outcomes taken once the input runs
/// out.
#[test]
fn input_does_not_starve_yielding_threads() {
    fn spin_lock(input: &[u8]) {
        Builder::new().check_with_input(input, || {
            let lock = Arc::new(loom::sync::Mutex::new(0));
            let threads: Vec<_> = (0..3)
                .map(|_| {
                    let lock = lock.clone();
                    thread::spawn(move || loop {
                        if let Ok(mut count) = lock.try_lock() {
                            *count += 1;
                            break;
                        }
                        thread::yield_now();
                    })
                })
                .collect();
            for thread in threads {
                thread.join().unwrap();
            }
            assert_eq!(*lock.lock().unwrap(), 3);
        });
    }

    fn chain(input: &[u8]) {
        Builder::new().check_with_input(input, || {
            let first = Arc::new(loom::sync::atomic::AtomicBool::new(false));
            let second = Arc::new(loom::sync::atomic::AtomicBool::new(false));
            let middle = {
                let (first, second) = (first.clone(), second.clone());
                thread::spawn(move || {
                    while !second.load(Ordering::Acquire) {
                        thread::yield_now();
                    }
                    first.store(true, Ordering::Release);
                })
            };
            let last = {
                let second = second.clone();
                thread::spawn(move || second.store(true, Ordering::Release))
            };
            while !first.load(Ordering::Acquire) {
                thread::yield_now();
            }
            middle.join().unwrap();
            last.join().unwrap();
        });
    }

    // Every repeated byte, and a few hundred mixed inputs.
    let mut inputs: Vec<Vec<u8>> = vec![Vec::new()];
    inputs.extend((0..=255).map(|byte| vec![byte; 16]));
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    for _ in 0..300 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let len = (seed % 65) as usize;
        inputs.push(
            seed.to_le_bytes()
                .iter()
                .copied()
                .cycle()
                .take(len)
                .collect(),
        );
    }

    for input in &inputs {
        spin_lock(input);
        chain(input);
    }
}
