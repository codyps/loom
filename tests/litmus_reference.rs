#![deny(warnings, rust_2018_idioms)]

//! Checks loom's memory model against a reference.
//!
//! `litmus_rc11/reference.txt` lists small programs together with the
//! outcomes that the RC11 memory model ("Repairing Sequential Consistency in
//! C/C++11", with C++20's release sequences) allows for them, as computed by
//! the `herd7` simulator. Each line is
//!
//! ```text
//! program | allowed outcomes | allowed outcomes loom is known not to find
//! ```
//!
//! with the last part usually absent. See `litmus_rc11/mod.rs` for the
//! notation. For every program:
//!
//! - loom must not produce an outcome that is not allowed. That would be a
//!   false alarm: a model failing on an execution that cannot happen.
//! - loom must produce every allowed outcome, except the ones listed as
//!   known gaps. A missing outcome is a bug loom would not find. The known
//!   gaps are there so that this is noticed when it gets worse, and so that
//!   the list has to be updated (shortened) when it gets better.

mod litmus_rc11;

use std::collections::BTreeSet;

/// What herd allows.
const REFERENCE: &str = include_str!("litmus_rc11/reference.txt");

/// What GenMC finds, for programs that herd cannot be given: ones with
/// locks, and ones it would take hours for.
const REFERENCE_GENMC: &str = include_str!("litmus_rc11/reference_genmc.txt");

/// The programs are split over a few tests to run in parallel.
const PARTS: usize = 8;

/// An unoptimized build takes minutes for all of the programs, and checks
/// one in this many. An optimized one checks every program.
const DEBUG_SAMPLE: usize = 8;

fn check(reference: &str, part: usize) {
    let mut failures = Vec::new();

    let sample = if cfg!(debug_assertions) {
        DEBUG_SAMPLE
    } else {
        1
    };

    for line in reference.lines().skip(part).step_by(PARTS).step_by(sample) {
        let mut fields = line.split(" | ");

        let program = fields.next().unwrap();
        let allowed: BTreeSet<&str> = fields.next().unwrap().split(' ').collect();
        let gaps: BTreeSet<&str> = fields
            .next()
            .map_or(BTreeSet::new(), |gaps| gaps.split(' ').collect());

        let found = litmus_rc11::outcomes(program);
        let found: BTreeSet<&str> = found.iter().map(String::as_str).collect();

        let forbidden: Vec<_> = found.difference(&allowed).collect();
        if !forbidden.is_empty() {
            failures.push(format!("{}: forbidden outcomes {:?}", program, forbidden));
        }

        let expected: BTreeSet<&str> = allowed.difference(&gaps).copied().collect();

        let missing: Vec<_> = expected.difference(&found).collect();
        if !missing.is_empty() {
            failures.push(format!("{}: missing outcomes {:?}", program, missing));
        }

        let closed: Vec<_> = found.intersection(&gaps).collect();
        if !closed.is_empty() {
            failures.push(format!(
                "{}: outcomes listed as known gaps are now found, update the reference: {:?}",
                program, closed
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} programs disagree with the reference:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

macro_rules! parts {
    ($($name:ident: $reference:expr, $part:expr,)*) => {
        $(
            #[test]
            fn $name() {
                check($reference, $part);
            }
        )*
    };
}

parts! {
    reference_part_0: REFERENCE, 0,
    reference_part_1: REFERENCE, 1,
    reference_part_2: REFERENCE, 2,
    reference_part_3: REFERENCE, 3,
    reference_part_4: REFERENCE, 4,
    reference_part_5: REFERENCE, 5,
    reference_part_6: REFERENCE, 6,
    reference_part_7: REFERENCE, 7,
    genmc_part_0: REFERENCE_GENMC, 0,
    genmc_part_1: REFERENCE_GENMC, 1,
    genmc_part_2: REFERENCE_GENMC, 2,
    genmc_part_3: REFERENCE_GENMC, 3,
    genmc_part_4: REFERENCE_GENMC, 4,
    genmc_part_5: REFERENCE_GENMC, 5,
    genmc_part_6: REFERENCE_GENMC, 6,
    genmc_part_7: REFERENCE_GENMC, 7,
}
