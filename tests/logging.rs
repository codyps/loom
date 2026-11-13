#![deny(warnings, rust_2018_idioms)]

use loom::sync::atomic::AtomicUsize;
use loom::thread;

use std::sync::atomic::{AtomicUsize as StdAtomicUsize, Ordering::SeqCst};
use std::sync::Arc;

use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// A subscriber that only counts the events it is given.
struct CountEvents(Arc<StdAtomicUsize>);

impl Subscriber for CountEvents {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, _: &Event<'_>) {
        self.0.fetch_add(1, SeqCst);
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

fn body() {
    let num = loom::sync::Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    thread::spawn(move || {
        num2.fetch_add(1, SeqCst);
    })
    .join()
    .unwrap();

    assert_eq!(num.load(SeqCst), 1);
}

/// A subscriber installed by the caller keeps receiving loom's events; loom
/// must not replace it with its own `LOOM_LOG` subscriber.
#[test]
fn builder_check_respects_existing_subscriber() {
    let events = Arc::new(StdAtomicUsize::new(0));

    tracing::subscriber::with_default(CountEvents(events.clone()), || {
        loom::model::Builder::new().check(body);
    });

    assert!(events.load(SeqCst) > 0, "caller's subscriber saw no events");
}

#[test]
fn model_respects_existing_subscriber() {
    let events = Arc::new(StdAtomicUsize::new(0));

    tracing::subscriber::with_default(CountEvents(events.clone()), || {
        loom::model(body);
    });

    assert!(events.load(SeqCst) > 0, "caller's subscriber saw no events");
}

/// Without a caller-provided subscriber both entry points still work (and
/// install loom's own subscriber).
#[test]
fn check_without_subscriber() {
    loom::model::Builder::new().check(body);
    loom::model(body);
}
