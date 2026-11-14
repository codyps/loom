//! A tiny interpreter for litmus programs, so that a large number of them
//! can be run under loom from a data file and compared with the outcomes a
//! reference model allows.
//!
//! A program is a list of threads separated by `;`, each a list of
//! operations separated by `,`:
//!
//! - `W<var><value><order>`: store, e.g. `Wx1s`
//! - `R<var><order>`: load, e.g. `Ryr`
//! - `X<var><value><order>`: exchange (swap), e.g. `Xx2s`
//! - `F`: `SeqCst` fence
//!
//! Variables are `x`, `y` and `z`, values single digits, and the orders are
//! `r` (relaxed), `a` (acquire for loads, release for stores, both for
//! exchanges) and `s` (`SeqCst`).
//!
//! An outcome lists the values read by each thread, in program order, and
//! the final value of each variable: `0:1,0|1:0|x=1,y=2,z=0`.

use loom::sync::atomic::Ordering::{self, AcqRel, Acquire, Relaxed, Release, SeqCst};
use loom::sync::atomic::{fence, AtomicUsize};
use loom::sync::Arc;
use loom::thread;

use std::collections::BTreeSet;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy)]
pub enum Op {
    Store(usize, usize, Ordering),
    Load(usize, Ordering),
    Exchange(usize, usize, Ordering),
    Fence,
}

pub const VARS: usize = 3;

pub fn parse(program: &str) -> Vec<Vec<Op>> {
    fn var(c: u8) -> usize {
        match c {
            b'x' => 0,
            b'y' => 1,
            b'z' => 2,
            _ => panic!("bad variable {:?}", c as char),
        }
    }

    fn value(c: u8) -> usize {
        assert!(c.is_ascii_digit(), "bad value {:?}", c as char);
        (c - b'0') as usize
    }

    program
        .split(';')
        .map(|thread| {
            thread
                .split(',')
                .map(|op| {
                    let op = op.trim().as_bytes();
                    let order = |c: u8, load: bool, store: bool| match (c, load, store) {
                        (b'r', ..) => Relaxed,
                        (b's', ..) => SeqCst,
                        (b'a', true, false) => Acquire,
                        (b'a', false, true) => Release,
                        (b'a', true, true) => AcqRel,
                        _ => panic!("bad order {:?}", c as char),
                    };

                    match op[0] {
                        b'W' => Op::Store(var(op[1]), value(op[2]), order(op[3], false, true)),
                        b'R' => Op::Load(var(op[1]), order(op[2], true, false)),
                        b'X' => Op::Exchange(var(op[1]), value(op[2]), order(op[3], true, true)),
                        b'F' => Op::Fence,
                        other => panic!("bad operation {:?}", other as char),
                    }
                })
                .collect()
        })
        .collect()
}

/// Runs `program` under loom and returns every outcome it produced.
#[allow(dead_code)]
pub fn outcomes(program: &str) -> BTreeSet<String> {
    outcomes_within(program, None).0
}

/// Like `outcomes`, but gives up after `budget`. Returns the outcomes found
/// and whether the exploration ran to completion.
pub fn outcomes_within(
    program: &str,
    budget: Option<std::time::Duration>,
) -> (BTreeSet<String>, bool) {
    let threads = parse(program);
    let seen = std::sync::Arc::new(Mutex::new(BTreeSet::new()));
    let start = std::time::Instant::now();

    {
        let seen = seen.clone();

        let mut builder = loom::model::Builder::new();
        // Litmus programs are tiny; make sure nothing cuts exploration short.
        // (`LOOM_LITMUS_PREEMPTIONS` is for comparing bounded searches with
        // each other, not with the reference results.)
        builder.preemption_bound = std::env::var("LOOM_LITMUS_PREEMPTIONS")
            .ok()
            .map(|bound| bound.parse().expect("invalid LOOM_LITMUS_PREEMPTIONS"));
        builder.max_branches = 10_000;
        builder.max_duration = budget;
        // The time limit is only looked at every `checkpoint_interval` executions.
        builder.checkpoint_interval = 500;

        builder.check(move || {
            let vars: Arc<[AtomicUsize; VARS]> = Arc::new([
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
            ]);

            let handles: Vec<_> = threads
                .iter()
                .cloned()
                .map(|ops| {
                    let vars = vars.clone();
                    thread::spawn(move || {
                        let mut read = Vec::new();

                        for op in ops {
                            match op {
                                Op::Store(var, value, order) => vars[var].store(value, order),
                                Op::Load(var, order) => read.push(vars[var].load(order)),
                                Op::Exchange(var, value, order) => {
                                    read.push(vars[var].swap(value, order))
                                }
                                Op::Fence => fence(SeqCst),
                            }
                        }

                        read
                    })
                })
                .collect();

            let mut outcome = String::new();

            for (i, handle) in handles.into_iter().enumerate() {
                let read: Vec<String> = handle
                    .join()
                    .unwrap()
                    .iter()
                    .map(|value| value.to_string())
                    .collect();

                outcome.push_str(&format!("{}:{}|", i, read.join(",")));
            }

            // Everything has been joined, so these read the final values.
            outcome.push_str(&format!(
                "x={},y={},z={}",
                vars[0].load(Relaxed),
                vars[1].load(Relaxed),
                vars[2].load(Relaxed)
            ));

            seen.lock().unwrap().insert(outcome);
        });
    }

    let complete = budget.map_or(true, |budget| start.elapsed() < budget);
    let seen = std::mem::take(&mut *seen.lock().unwrap());

    (seen, complete)
}
