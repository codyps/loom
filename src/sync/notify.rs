use crate::rt;

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

/// Notifies a waiter to wake up.
///
/// This provides the API of `tokio::sync::Notify`: [`notify_one`],
/// [`notify_last`], [`notify_waiters`] and the [`notified`] future, with the
/// same permit semantics. A `Notify` holds at most one permit. `notify_one`
/// hands it to a waiter if there is one, and otherwise stores it for the next
/// waiter to pick up; `notify_waiters` wakes everything currently waiting and
/// never stores a permit.
///
/// In addition a thread can block on the same notifications with [`wait`],
/// which implements the park / unpark pattern. Any number of threads may be
/// blocked in `wait`, and any number of [`notified`] futures be pending,
/// together.
///
/// Every operation is a point at which loom may switch threads, and
/// notification establishes an acquire / release synchronization point
/// between the notifier and the notified.
///
/// Using this type is useful to mock out constructs when using loom tests.
///
/// [`notify_one`]: Notify::notify_one
/// [`notify_last`]: Notify::notify_last
/// [`notify_waiters`]: Notify::notify_waiters
/// [`notified`]: Notify::notified
/// [`wait`]: Notify::wait
#[derive(Debug)]
pub struct Notify {
    /// What wakes a thread blocked in `wait`, one per thread that has
    /// waited, by thread.
    threads: std::sync::Mutex<[Option<rt::Notify>; crate::MAX_THREADS]>,

    /// Modeled lock around `state`. Taking it is what makes each operation
    /// visible to loom, both as a branch point and for causality.
    lock: rt::Mutex,

    /// Only accessed with `lock` held.
    state: std::sync::Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// A notification that arrived while nothing was waiting.
    permit: bool,

    /// Number of `notify_waiters` calls so far. A `Notified` future is woken
    /// by every such call made after the future was created, even if it has
    /// not been polled yet.
    notify_waiters_calls: usize,

    next_waiter: usize,

    /// Registered waiters that have not been notified, oldest first.
    waiters: VecDeque<Waiter>,

    /// Waiters that have been notified but have not noticed yet.
    delivered: Vec<(usize, Notification)>,
}

#[derive(Debug)]
struct Waiter {
    id: usize,
    wake: Wake,
}

/// How to tell a waiter that it has been notified.
#[derive(Debug)]
enum Wake {
    /// A thread blocked in `Notify::wait`, through its object.
    Thread(rt::Notify),

    /// A `Notified` future; it may not have a waker yet if it was only
    /// enabled.
    Task(Option<Waker>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Notification {
    /// From `notify_one` / `notify_last`. If the waiter goes away without
    /// consuming it, it is passed on.
    One,

    /// From `notify_waiters`.
    All,
}

/// Future returned from [`Notify::notified()`].
///
/// Mock implementation of `tokio::sync::futures::Notified`.
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Notified<'a> {
    notify: &'a Notify,
    state: NotifiedState,
}

#[derive(Debug)]
enum NotifiedState {
    /// Not registered yet. Holds the number of `notify_waiters` calls at the
    /// time the future was created.
    Init(usize),

    /// Registered as the waiter with this id.
    Waiting(usize),

    Done,
}

impl Notify {
    /// Create a new `Notify`.
    pub fn new() -> Notify {
        Notify {
            threads: std::sync::Mutex::new([None; crate::MAX_THREADS]),
            lock: rt::Mutex::new(true),
            state: std::sync::Mutex::new(State {
                permit: false,
                notify_waiters_calls: 0,
                next_waiter: 0,
                waiters: VecDeque::new(),
                delivered: Vec::new(),
            }),
        }
    }

    /// Notify the waiter.
    ///
    /// This is [`notify_one`](Notify::notify_one) under its original name.
    #[track_caller]
    pub fn notify(&self) {
        self.notify_one();
    }

    /// Notifies the first waiting task.
    ///
    /// Behaves like `tokio::sync::Notify::notify_one`: if something is
    /// waiting, the waiter that has been waiting the longest is notified.
    /// Otherwise a permit is stored, to be consumed by the next
    /// [`notified()`](Notify::notified) future or call to
    /// [`wait()`](Notify::wait). At most one permit is stored.
    #[track_caller]
    pub fn notify_one(&self) {
        let location = location!();
        let wake = self.with_state(location, |state| state.notify_one(false));

        self.wake(wake, location);
    }

    /// Notifies the last waiting task.
    ///
    /// Behaves like `tokio::sync::Notify::notify_last`: the same as
    /// [`notify_one()`](Notify::notify_one), except that the waiter that
    /// started waiting most recently is the one notified.
    #[track_caller]
    pub fn notify_last(&self) {
        let location = location!();
        let wake = self.with_state(location, |state| state.notify_one(true));

        self.wake(wake, location);
    }

    /// Notifies all waiting tasks.
    ///
    /// Behaves like `tokio::sync::Notify::notify_waiters`: everything
    /// currently waiting is notified, including [`Notified`] futures that
    /// have been created but not polled yet. Unlike
    /// [`notify_one()`](Notify::notify_one), no permit is stored if nothing
    /// is waiting.
    #[track_caller]
    pub fn notify_waiters(&self) {
        let location = location!();

        let wakes: Vec<Wake> = self.with_state(location, |state| {
            state.notify_waiters_calls += 1;

            let waiters = std::mem::take(&mut state.waiters);
            waiters
                .into_iter()
                .map(|waiter| {
                    state.delivered.push((waiter.id, Notification::All));
                    waiter.wake
                })
                .collect()
        });

        for wake in wakes {
            self.wake(Some(wake), location);
        }
    }

    /// Wait for a notification.
    ///
    /// Behaves like `tokio::sync::Notify::notified`: the returned future
    /// completes when it is notified, right away if a permit is stored.
    ///
    /// The future starts receiving [`notify_waiters`](Notify::notify_waiters)
    /// notifications when it is created, and [`notify_one`](Notify::notify_one)
    /// notifications once it has been polled (or
    /// [enabled](Notified::enable)) for the first time.
    #[track_caller]
    pub fn notified(&self) -> Notified<'_> {
        let calls = self.with_state(location!(), |state| state.notify_waiters_calls);

        Notified {
            notify: self,
            state: NotifiedState::Init(calls),
        }
    }

    /// Block the current thread until it is notified.
    ///
    /// Consumes a stored permit, returning immediately, if there is one. The
    /// wait may also end spuriously, as with `std::thread::park`: at most
    /// once per thread and `Notify` in an execution, so that a loop around
    /// it ends.
    #[track_caller]
    pub fn wait(&self) {
        let location = location!();
        let object = self.thread_object();

        let waiter = self.with_state(location, |state| {
            if state.permit {
                state.permit = false;
                None
            } else {
                Some(state.register(Wake::Thread(object)))
            }
        });

        if let Some(id) = waiter {
            object.wait(location);

            // If the wait ended without a notification, stop waiting.
            self.with_state(location, |state| {
                if state.take_delivered(id).is_none() {
                    state.unregister(id);
                }
            });
        }
    }

    /// What wakes the current thread when it waits.
    fn thread_object(&self) -> rt::Notify {
        let thread = rt::execution(|execution| execution.threads.active_id().as_usize());
        let mut threads = self.threads.lock().unwrap();

        *threads[thread].get_or_insert_with(|| rt::Notify::new(false, true))
    }

    fn with_state<R>(&self, location: rt::Location, f: impl FnOnce(&mut State) -> R) -> R {
        self.lock.acquire_lock(location);
        let ret = f(&mut self.state.lock().unwrap());
        self.lock.release_lock();

        ret
    }

    /// Wakes a notified waiter. Must not be called with `lock` held: waking
    /// is itself a modeled operation.
    fn wake(&self, wake: Option<Wake>, location: rt::Location) {
        match wake {
            Some(Wake::Thread(object)) => object.notify(location),
            Some(Wake::Task(Some(waker))) => waker.wake(),
            Some(Wake::Task(None)) | None => {}
        }
    }
}

impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    fn register(&mut self, wake: Wake) -> usize {
        let id = self.next_waiter;
        self.next_waiter += 1;
        self.waiters.push_back(Waiter { id, wake });

        id
    }

    /// Returns `true` if the waiter was still registered.
    fn unregister(&mut self, id: usize) -> bool {
        let len = self.waiters.len();
        self.waiters.retain(|waiter| waiter.id != id);

        self.waiters.len() != len
    }

    fn take_delivered(&mut self, id: usize) -> Option<Notification> {
        let index = self
            .delivered
            .iter()
            .position(|(waiter, _)| *waiter == id)?;

        Some(self.delivered.swap_remove(index).1)
    }

    /// Notifies one waiter, or stores a permit if there is none.
    fn notify_one(&mut self, last: bool) -> Option<Wake> {
        let waiter = if last {
            self.waiters.pop_back()
        } else {
            self.waiters.pop_front()
        };

        match waiter {
            Some(waiter) => {
                self.delivered.push((waiter.id, Notification::One));
                Some(waiter.wake)
            }
            None => {
                self.permit = true;
                None
            }
        }
    }
}

impl Notified<'_> {
    /// Adds this future to the list of futures that are ready to receive
    /// wakeups from calls to [`notify_one`](Notify::notify_one), without
    /// polling it.
    ///
    /// Behaves like `tokio::sync::futures::Notified::enable`. Returns `true`
    /// if the future has already been notified (or consumed a stored
    /// permit), in which case awaiting it completes immediately.
    #[track_caller]
    pub fn enable(self: Pin<&mut Self>) -> bool {
        self.get_mut().poll_notified(None, location!()).is_ready()
    }

    fn poll_notified(&mut self, waker: Option<&Waker>, location: rt::Location) -> Poll<()> {
        let next = match self.state {
            NotifiedState::Init(calls) => self.notify.with_state(location, |state| {
                if state.notify_waiters_calls != calls {
                    // `notify_waiters` was called since the future was created.
                    NotifiedState::Done
                } else if state.permit {
                    state.permit = false;
                    NotifiedState::Done
                } else {
                    NotifiedState::Waiting(state.register(Wake::Task(waker.cloned())))
                }
            }),
            NotifiedState::Waiting(id) => self.notify.with_state(location, |state| {
                if state.take_delivered(id).is_some() {
                    return NotifiedState::Done;
                }

                // Still waiting: make sure the latest waker gets woken.
                if let Some(waker) = waker {
                    for waiter in &mut state.waiters {
                        if waiter.id == id {
                            match &waiter.wake {
                                Wake::Task(Some(current)) if current.will_wake(waker) => {}
                                _ => waiter.wake = Wake::Task(Some(waker.clone())),
                            }
                        }
                    }
                }

                NotifiedState::Waiting(id)
            }),
            NotifiedState::Done => NotifiedState::Done,
        };

        self.state = next;

        match self.state {
            NotifiedState::Done => Poll::Ready(()),
            _ => Poll::Pending,
        }
    }
}

impl Future for Notified<'_> {
    type Output = ();

    #[track_caller]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.get_mut().poll_notified(Some(cx.waker()), location!())
    }
}

impl Drop for Notified<'_> {
    #[track_caller]
    fn drop(&mut self) {
        let id = match self.state {
            NotifiedState::Waiting(id) => id,
            _ => return,
        };

        // Deregistering is a modeled operation, which must not be attempted
        // while the model is being torn down by a panic.
        if std::thread::panicking() {
            return;
        }

        let location = location!();

        let wake = self.notify.with_state(location, |state| {
            if state.unregister(id) {
                return None;
            }

            // Already notified, but the notification will never be observed.
            // One meant for a single waiter is passed on to the next one.
            match state.take_delivered(id) {
                Some(Notification::One) => state.notify_one(false),
                _ => None,
            }
        });

        self.notify.wake(wake, location);
    }
}
