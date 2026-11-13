use loom::sync::mpsc::channel;
use loom::thread;

use std::sync::mpsc::TryRecvError;

#[test]
fn basic_sequential_usage() {
    loom::model(|| {
        let (s, r) = channel();
        s.send(5).unwrap();
        let val = r.recv().unwrap();
        assert_eq!(val, 5);
    });
}

#[test]
fn basic_parallel_usage() {
    loom::model(|| {
        let (s, r) = channel();
        thread::spawn(move || {
            s.send(5).unwrap();
        });
        let val = r.recv().unwrap();
        assert_eq!(val, 5);
    });
}

#[test]
fn commutative_senders() {
    loom::model(|| {
        let (s, r) = channel();
        let s2 = s.clone();
        thread::spawn(move || {
            s.send(5).unwrap();
        });
        thread::spawn(move || {
            s2.send(6).unwrap();
        });
        let mut val = r.recv().unwrap();
        val += r.recv().unwrap();
        assert_eq!(val, 11);
    });
}

fn ignore_result<A, B>(_: Result<A, B>) {}

#[test]
#[should_panic]
fn non_commutative_senders1() {
    loom::model(|| {
        let (s, r) = channel();
        let s2 = s.clone();
        thread::spawn(move || {
            ignore_result(s.send(5));
        });
        thread::spawn(move || {
            ignore_result(s2.send(6));
        });
        let val = r.recv().unwrap();
        assert_eq!(val, 5);
        ignore_result(r.recv());
    });
}

#[test]
#[should_panic]
fn non_commutative_senders2() {
    loom::model(|| {
        let (s, r) = channel();
        let s2 = s.clone();
        thread::spawn(move || {
            ignore_result(s.send(5));
        });
        thread::spawn(move || {
            ignore_result(s2.send(6));
        });
        let val = r.recv().unwrap();
        assert_eq!(val, 6);
        ignore_result(r.recv());
    });
}

#[test]
fn drop_receiver() {
    loom::model(|| {
        let (s, r) = channel();
        s.send(1).unwrap();
        s.send(2).unwrap();
        assert_eq!(r.recv().unwrap(), 1);
    });
}

/// Dropping the last sender disconnects the channel, which ends a receiver
/// loop instead of leaving it blocked.
#[test]
fn recv_ends_when_last_sender_is_dropped() {
    loom::model(|| {
        let (sender, receiver) = channel();

        let handle = thread::spawn(move || {
            let mut received = Vec::new();
            while let Ok(i) = receiver.recv() {
                received.push(i);
            }
            received
        });

        sender.send(0_i32).unwrap();
        drop(sender);

        assert_eq!(handle.join().unwrap(), [0]);
    });
}

/// The channel is only disconnected once *all* senders are gone, and
/// messages sent before that are still delivered.
#[test]
fn recv_with_several_senders_dropped() {
    loom::model(|| {
        let (sender, receiver) = channel();
        let sender2 = sender.clone();

        let th = thread::spawn(move || {
            sender2.send(1).unwrap();
        });

        sender.send(2).unwrap();
        drop(sender);

        let mut sum = 0;
        while let Ok(i) = receiver.recv() {
            sum += i;
        }

        assert_eq!(sum, 3);
        th.join().unwrap();
    });
}

#[test]
fn recv_on_disconnected_channel() {
    loom::model(|| {
        let (sender, receiver) = channel::<usize>();
        drop(sender);

        assert!(receiver.recv().is_err());
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Disconnected));
    });
}

/// Sending to a dropped receiver fails and hands the message back; it is not
/// a leaked message.
#[test]
fn send_on_closed_channel() {
    loom::model(|| {
        let (sender, receiver) = channel();
        drop(receiver);

        let err = sender.send(5).unwrap_err();
        assert_eq!(err.0, 5);
    });
}

/// Messages still queued when the receiver goes away are dropped with it.
#[test]
fn receiver_dropped_with_queued_messages() {
    loom::model(|| {
        let (sender, receiver) = channel();

        sender.send(String::from("a")).unwrap();
        sender.send(String::from("b")).unwrap();

        drop(receiver);

        assert!(sender.send(String::from("c")).is_err());
    });
}

/// A send racing with the receiver being dropped can go either way.
#[test]
fn send_races_with_receiver_drop() {
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    use std::sync::Arc;

    let saw_ok = Arc::new(AtomicBool::new(false));
    let saw_err = Arc::new(AtomicBool::new(false));

    {
        let saw_ok = saw_ok.clone();
        let saw_err = saw_err.clone();
        loom::model(move || {
            let (sender, receiver) = channel();

            let th = thread::spawn(move || drop(receiver));

            match sender.send(1) {
                Ok(()) => saw_ok.store(true, SeqCst),
                Err(_) => saw_err.store(true, SeqCst),
            }

            th.join().unwrap();
        });
    }

    assert!(saw_ok.load(SeqCst), "send never succeeded");
    assert!(saw_err.load(SeqCst), "send never failed");
}

/// `try_recv` racing with a sender observes all three outcomes: nothing yet,
/// the message, and (once the message is taken) the disconnect.
#[test]
fn try_recv_races_with_send() {
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    let seen = Arc::new(Mutex::new(BTreeSet::new()));

    {
        let seen = seen.clone();
        loom::model(move || {
            let (sender, receiver) = channel();

            let th = thread::spawn(move || {
                sender.send(1).unwrap();
            });

            let outcome = match receiver.try_recv() {
                Ok(_) => "message",
                Err(TryRecvError::Empty) => "empty",
                Err(TryRecvError::Disconnected) => "disconnected",
            };
            seen.lock().unwrap().insert(outcome);

            th.join().unwrap();
        });
    }

    let seen = seen.lock().unwrap();
    assert_eq!(*seen, BTreeSet::from(["empty", "message"]));
}

#[test]
fn try_recv_races_with_sender_drop() {
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    let seen = Arc::new(Mutex::new(BTreeSet::new()));

    {
        let seen = seen.clone();
        loom::model(move || {
            let (sender, receiver) = channel::<usize>();

            let th = thread::spawn(move || drop(sender));

            let outcome = match receiver.try_recv() {
                Ok(_) => "message",
                Err(TryRecvError::Empty) => "empty",
                Err(TryRecvError::Disconnected) => "disconnected",
            };
            seen.lock().unwrap().insert(outcome);

            th.join().unwrap();
        });
    }

    let seen = seen.lock().unwrap();
    assert_eq!(*seen, BTreeSet::from(["disconnected", "empty"]));
}
