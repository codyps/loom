//! Runs litmus programs from a file under loom and writes the outcomes to
//! another file, for comparison with a reference model. Not a test in itself:
//! it does nothing unless `LOOM_LITMUS_IN` and `LOOM_LITMUS_OUT` are set.
//!
//! Input: one program per line. Output: `program => outcome outcome ...`
//! (`~>` if `LOOM_LITMUS_BUDGET` seconds ran out before exploration finished).

mod litmus_rc11;

use std::io::Write;

#[test]
fn run_litmus_file() {
    let (input, output) = match (
        std::env::var("LOOM_LITMUS_IN"),
        std::env::var("LOOM_LITMUS_OUT"),
    ) {
        (Ok(input), Ok(output)) => (input, output),
        _ => return,
    };

    // Optional limit, in seconds, on the time spent on one program.
    let budget = std::env::var("LOOM_LITMUS_BUDGET")
        .ok()
        .map(|secs| std::time::Duration::from_secs(secs.parse().unwrap()));

    let programs = std::fs::read_to_string(input).unwrap();
    let mut out = std::io::BufWriter::new(std::fs::File::create(output).unwrap());

    for program in programs.lines().filter(|line| !line.trim().is_empty()) {
        let program = program.trim();

        // A panic inside the model is a result worth recording too.
        let result = std::panic::catch_unwind(|| litmus_rc11::outcomes_within(program, budget));

        match result {
            Ok((outcomes, complete)) => {
                let outcomes: Vec<String> = outcomes.into_iter().collect();
                // `~>` marks a program whose exploration was cut short.
                let arrow = if complete { "=>" } else { "~>" };
                writeln!(out, "{} {} {}", program, arrow, outcomes.join(" ")).unwrap();
            }
            Err(_) => writeln!(out, "{} => PANIC", program).unwrap(),
        }

        out.flush().unwrap();
    }
}
