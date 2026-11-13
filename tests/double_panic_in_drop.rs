#[test]
#[should_panic]
fn double_panic_at_branch_max() {
    let mut builder = loom::model::Builder::new();
    builder.max_branches = 2;

    builder.check(|| {
        let _arc = loom::sync::Arc::new(());
        loom::thread::yield_now();
        loom::thread::yield_now();
        loom::thread::yield_now();
    });
}

/// A thread that has started running and is suspended while holding loom
/// objects must not turn a panic on the main thread into an abort.
#[test]
#[should_panic(expected = "boom")]
fn panic_while_started_thread_holds_loom_objects() {
    use loom::sync::atomic::AtomicUsize;
    use loom::sync::Arc;
    use loom::thread;

    loom::model(|| {
        let arc = Arc::new(AtomicUsize::new(0));
        let arc2 = arc.clone();

        thread::spawn(move || {
            let _arc = arc2;
            loop {
                thread::yield_now();
            }
        });

        thread::yield_now();
        thread::yield_now();

        panic!("boom");
    });
}
