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

const REFERENCE: &str = include_str!("litmus_rc11/reference.txt");

/// The programs are split over a few tests to run in parallel.
const PARTS: usize = 8;

fn check(part: usize) {
    let mut failures = Vec::new();

    for line in REFERENCE.lines().skip(part).step_by(PARTS) {
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
    ($($name:ident: $part:expr,)*) => {
        $(
            #[test]
            fn $name() {
                check($part);
            }
        )*
    };
}

parts! {
    reference_part_0: 0,
    reference_part_1: 1,
    reference_part_2: 2,
    reference_part_3: 3,
    reference_part_4: 4,
    reference_part_5: 5,
    reference_part_6: 6,
    reference_part_7: 7,
}
