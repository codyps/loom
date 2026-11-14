use crate::rt::{object, Access, Location, Synchronize, VersionVec, MAX_THREADS};
use std::collections::VecDeque;
use std::sync::atomic::Ordering::{Acquire, Release};

#[derive(Debug)]
pub(crate) struct Channel {
    state: object::Ref<State>,
}

#[derive(Debug)]
pub(super) struct State {
    /// Count of messages in the channel.
    msg_cnt: usize,

    /// Number of `Sender` handles that have not been dropped. Once this
    /// reaches zero and the channel is empty, receiving fails instead of
    /// blocking.
    senders: usize,

    /// `false` once the `Receiver` has been dropped, after which sending
    /// fails.
    receiver_alive: bool,

    /// Last access that was a send operation.
    last_send_access: Option<Access>,
    /// Each thread's last drop of a sender.
    last_sender_drop: [Option<Access>; MAX_THREADS],
    /// Last access that was a receive operation.
    last_recv_access: Option<Access>,
    /// Last access by the receiver that observed whether a message was
    /// there without waiting for one: a `try_recv`, or the receiver's drop.
    last_observe_access: Option<Access>,

    /// A synchronization point for synchronizing the sending threads and the
    /// channel.
    ///
    /// The `mpsc` channels have a guarantee that the messages will be received
    /// in the same order in which they were sent. Therefore, if thread `t1`
    /// managed to send `m1` before `t2` sent `m2`, the thread that received
    /// `m2` can be sure that `m1` was already sent and received. In other
    /// words, it is sound for the receiver of `m2` to know that `m1` happened
    /// before `m2`. That is why we have a single `sender_synchronize` for
    /// senders which we use to "timestamp" each message put in the channel.
    /// However, in our example, the receiver of `m1` does not know whether `m2`
    /// was already sent or not and, therefore, by reading from the channel it
    /// should not learn any facts about `happens_before(send(m2), recv(m1))`.
    /// That is why we cannot use single `Synchronize` for the entire channel
    /// and on the receiver side we need to use `Synchronize` per message.
    sender_synchronize: Synchronize,
    /// A synchronization point per message synchronizing the receiving thread
    /// with the channel state at the point when the received message was sent.
    receiver_synchronize: VecDeque<Synchronize>,

    created: Location,
}

/// Actions performed on the Channel.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(super) enum Action {
    /// Send a message
    MsgSend,
    /// Receive a message, waiting for one if necessary
    MsgRecv,
    /// Receive a message if there is one; never waits.
    MsgTryRecv,
    /// Drop a sender
    SenderDrop,
    /// Drop the receiver
    ReceiverDrop,
}

/// What a receive operation found.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum Recv {
    /// A message was taken out of the channel.
    Message,
    /// The channel is empty, but there are senders left.
    Empty,
    /// The channel is empty and all senders are gone.
    Disconnected,
}

impl Channel {
    pub(crate) fn new(location: Location) -> Self {
        super::execution(|execution| {
            let state = execution.objects.insert(State {
                msg_cnt: 0,
                senders: 1,
                receiver_alive: true,
                last_send_access: None,
                last_sender_drop: Default::default(),
                last_recv_access: None,
                last_observe_access: None,
                sender_synchronize: Synchronize::new(),
                receiver_synchronize: VecDeque::new(),
                created: location,
            });

            tracing::trace!(?state, %location, "mpsc::channel");
            Self { state }
        })
    }

    /// Returns `false` if the receiver is gone, in which case the message is
    /// not delivered.
    pub(crate) fn send(&self, location: Location) -> bool {
        self.state.branch_action(Action::MsgSend, location);
        super::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);

            if !state.receiver_alive {
                return false;
            }

            state.msg_cnt = state.msg_cnt.checked_add(1).expect("overflow");

            state
                .sender_synchronize
                .sync_store(&mut execution.threads, Release);
            state
                .receiver_synchronize
                .push_back(state.sender_synchronize);

            if state.msg_cnt == 1 {
                self.unblock_receivers(execution);
            }

            true
        })
    }

    /// Track a new sender handle.
    pub(crate) fn clone_sender(&self) {
        super::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);
            state.senders = state.senders.checked_add(1).expect("overflow");
        })
    }

    /// Track a sender handle going away. Dropping the last one disconnects
    /// the channel, which a receiver can observe.
    pub(crate) fn drop_sender(&self, location: Location) {
        // The drop may be running because of a panic, or after the execution
        // has come to an end, in which case it cannot be a branch point.
        if can_branch() {
            self.state.branch_action(Action::SenderDrop, location);
        }

        super::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);
            state.senders = state
                .senders
                .checked_sub(1)
                .expect("[loom internal bug] sender dropped twice");

            if state.senders == 0 && execution.threads.is_active() {
                // What the receiver learns from the disconnect is ordered
                // after everything the senders did.
                state
                    .sender_synchronize
                    .sync_store(&mut execution.threads, Release);

                // A receiver waiting for a message is not going to get one;
                // let it find out.
                self.unblock_receivers(execution);
            }
        })
    }

    /// Track the receiver going away. Messages still in the channel are
    /// dropped with it.
    pub(crate) fn drop_receiver(&self, location: Location) {
        if can_branch() {
            self.state.branch_action(Action::ReceiverDrop, location);
        }

        super::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);

            state.receiver_alive = false;
            state.msg_cnt = 0;

            let pending = std::mem::take(&mut state.receiver_synchronize);

            if execution.threads.is_active() {
                // The messages are destroyed by this thread, which makes it
                // their receiver as far as causality goes.
                for mut synchronize in pending {
                    synchronize.sync_load(&mut execution.threads, Acquire);
                }
            }
        })
    }

    /// Receive a message, blocking the thread until there is one or the
    /// channel is disconnected.
    pub(crate) fn recv(&self, location: Location) -> Recv {
        self.state
            .branch_disable(Action::MsgRecv, self.would_block(), location);

        let recv = self.take_message();

        assert_ne!(recv, Recv::Empty, "expected to be able to read the message");

        recv
    }

    /// Receive a message if there is one.
    pub(crate) fn try_recv(&self, location: Location) -> Recv {
        self.state.branch_action(Action::MsgTryRecv, location);
        self.take_message()
    }

    fn take_message(&self) -> Recv {
        super::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);
            let thread_id = execution.threads.active_id();

            if state.msg_cnt == 0 {
                if state.senders > 0 {
                    return Recv::Empty;
                }

                // Learn about the disconnect.
                let mut synchronize = state.sender_synchronize;
                synchronize.sync_load(&mut execution.threads, Acquire);

                return Recv::Disconnected;
            }

            state.msg_cnt -= 1;

            let mut synchronize = state.receiver_synchronize.pop_front().unwrap();
            synchronize.sync_load(&mut execution.threads, Acquire);

            if state.msg_cnt == 0 && state.senders > 0 {
                // Block all **other** threads waiting to read from the channel
                for (id, thread) in execution.threads.iter_mut() {
                    if id == thread_id {
                        continue;
                    }

                    if let Some(operation) = thread.operation.as_ref() {
                        if operation.object() == self.state.erase()
                            && operation.action() == object::Action::Channel(Action::MsgRecv)
                        {
                            let location = operation.location();
                            thread.set_blocked(location);
                        }
                    }
                }
            }

            Recv::Message
        })
    }

    /// Unblock all threads that are blocked waiting on this channel
    fn unblock_receivers(&self, execution: &mut super::Execution) {
        let thread_id = execution.threads.active_id();

        for (id, thread) in execution.threads.iter_mut() {
            if id == thread_id {
                continue;
            }

            let obj = thread
                .operation
                .as_ref()
                .map(|operation| operation.object());

            if obj == Some(self.state.erase()) {
                thread.set_runnable();
            }
        }
    }

    /// Returns `true` if a blocking receive has to wait: the channel is
    /// empty, and a sender could still put something in it.
    fn would_block(&self) -> bool {
        super::execution(|execution| {
            let state = self.state.get(&execution.objects);
            state.msg_cnt == 0 && state.senders > 0
        })
    }
}

/// Returns `true` if the current thread can take a branch point: it is not
/// unwinding, and the execution has a thread running.
fn can_branch() -> bool {
    !std::thread::panicking() && super::execution(|execution| execution.threads.is_active())
}

impl State {
    pub(super) fn check_for_leaks(&self, index: usize) {
        if self.msg_cnt != 0 {
            if self.created.is_captured() {
                panic!(
                    "Messages leaked.\n  \
                    Channel created: {}\n            \
                    Index: {}\n        \
                    Messages: {}",
                    self.created, index, self.msg_cnt
                );
            } else {
                panic!(
                    "Messages leaked.\n     Index: {}\n  Messages: {}",
                    index, self.msg_cnt
                );
            }
        }
    }

    /// Returns the previous accesses that `action` is dependent with.
    ///
    /// Sends are ordered among themselves, as are the receiver's operations.
    /// A blocking receive never observes a send "not having happened yet",
    /// it just waits, so the two do not race. A `try_recv` does observe
    /// that, as does dropping the receiver (which destroys what was sent
    /// before it and fails what is sent after it), so those race with sends,
    /// in both directions.
    ///
    /// Dropping a sender matters to nobody else, except for the drop of the
    /// last one: that disconnects the channel, which the receiver can
    /// observe like a send, and it hands the receiver what its thread
    /// knows. Which sender is the last to go is decided by the order of the
    /// drops, so the drop that disconnects is dependent with the last drop
    /// of every other thread.
    pub(super) fn dependent_accesses(&self, action: Action) -> impl Iterator<Item = &Access> {
        let none = &[][..];

        let (first, second, drops) = match action {
            Action::MsgSend => (&self.last_send_access, &self.last_observe_access, none),
            Action::SenderDrop if self.next_drop_disconnects() => {
                (&None, &self.last_observe_access, &self.last_sender_drop[..])
            }
            Action::SenderDrop => (&None, &None, none),
            Action::MsgRecv => (&self.last_recv_access, &None, none),
            Action::MsgTryRecv | Action::ReceiverDrop => (
                &self.last_recv_access,
                &self.last_send_access,
                &self.last_sender_drop[..],
            ),
        };

        first
            .iter()
            .chain(second.iter())
            .chain(drops.iter().flatten())
    }

    /// Returns `true` if the next drop of a sender disconnects the channel.
    fn next_drop_disconnects(&self) -> bool {
        self.senders == 1
    }

    /// Returns `true` if `action` has to be tried before each of the
    /// accesses it is dependent with, and not only before the most recent
    /// one. That goes for the drop that disconnects the channel: moved
    /// before another drop it is not the last one anymore, and has nothing
    /// left to be reordered with. See `rt::arc`, where the drop that frees
    /// the value is in the same position.
    pub(super) fn races_with_all(&self, action: Action) -> bool {
        action == Action::SenderDrop && self.next_drop_disconnects()
    }

    /// Returns `true` unless two operations that different threads are about
    /// to execute lead to the same state in either order.
    pub(super) fn dependent(&self, a: Action, b: Action) -> bool {
        use Action::*;

        let is_send = |action| matches!(action, MsgSend | SenderDrop);
        let is_recv = |action| matches!(action, MsgRecv | MsgTryRecv);

        match (a, b) {
            // The order of messages is the order in which they were sent.
            (MsgSend, MsgSend) => true,
            // A sender going away does not matter to the others, unless it
            // is the last one, which disconnects the channel.
            (MsgSend, SenderDrop) | (SenderDrop, MsgSend) => self.senders <= 1,
            // Which of the last two goes last decides whose knowledge the
            // receiver gets along with the disconnect.
            (SenderDrop, SenderDrop) => self.senders <= 2,
            // A receive takes a message from the front, a send adds one at
            // the back: as long as there is a message, they do not get in
            // each other's way. With the channel empty the receive finds
            // nothing, or the end of the channel, depending on what the
            // senders have done.
            (a, b) if (is_recv(a) && is_send(b)) || (is_send(a) && is_recv(b)) => self.msg_cnt == 0,
            _ => true,
        }
    }

    pub(super) fn set_last_access(
        &mut self,
        action: Action,
        thread: usize,
        path_id: usize,
        version: &VersionVec,
    ) {
        match action {
            Action::MsgSend => Access::set_or_create(&mut self.last_send_access, path_id, version),
            Action::SenderDrop => {
                Access::set_or_create(&mut self.last_sender_drop[thread], path_id, version)
            }
            Action::MsgRecv => Access::set_or_create(&mut self.last_recv_access, path_id, version),
            Action::MsgTryRecv | Action::ReceiverDrop => {
                Access::set_or_create(&mut self.last_recv_access, path_id, version);
                Access::set_or_create(&mut self.last_observe_access, path_id, version);
            }
        }
    }
}
