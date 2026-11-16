#![allow(deprecated)]

use crate::rt::Execution;

use generator::{self, Generator, Gn};
use scoped_tls::scoped_thread_local;
use std::cell::RefCell;
use std::collections::VecDeque;

pub(crate) struct Scheduler {
    max_threads: usize,

    /// Coroutines left over from earlier executions, each with the stack
    /// size it was created with. They have run their thread to its end and
    /// are waiting to be handed another one.
    ///
    /// Setting up a coroutine means mapping a stack and its guard page, and
    /// finishing one means unwinding it. A model is executed a great many
    /// times with the same few threads, and doing both for every thread of
    /// every execution took most of the time of a small model.
    idle: Vec<(Option<usize>, Thread)>,
}

type Thread = Generator<'static, Option<Box<dyn FnOnce()>>, ()>;

scoped_thread_local! {
    static STATE: RefCell<State<'_>>
}

struct QueuedSpawn {
    f: Box<dyn FnOnce()>,
    stack_size: Option<usize>,
}

struct State<'a> {
    execution: &'a mut Execution,
    queued_spawn: &'a mut VecDeque<QueuedSpawn>,
}

impl Scheduler {
    /// Create an execution
    pub(crate) fn new(capacity: usize) -> Scheduler {
        Scheduler {
            max_threads: capacity,
            idle: Vec::new(),
        }
    }

    /// Access the execution
    pub(crate) fn with_execution<F, R>(f: F) -> R
    where
        F: FnOnce(&mut Execution) -> R,
    {
        Self::with_state(|state| f(state.execution))
    }

    /// Access the execution, if there is one and it is not already being
    /// accessed further up the stack.
    ///
    /// For use from `Drop` implementations, which can run after a model has
    /// finished or while the runtime is in the middle of an operation.
    pub(crate) fn try_with_execution<F, R>(f: F) -> Option<R>
    where
        F: FnOnce(&mut Execution) -> R,
    {
        if !STATE.is_set() {
            return None;
        }

        STATE.with(|state| {
            let mut state = state.try_borrow_mut().ok()?;
            Some(f(state.execution))
        })
    }

    /// Perform a context switch
    pub(crate) fn switch() {
        use std::future::Future;
        use std::pin::Pin;
        use std::ptr;
        use std::task::{Context, RawWaker, RawWakerVTable, Waker};

        unsafe fn noop_clone(_: *const ()) -> RawWaker {
            unreachable!()
        }
        unsafe fn noop(_: *const ()) {}

        // Wrapping with an async block deals with the thread-local context
        // `std` uses to manage async blocks
        let mut switch = async { generator::yield_with(()) };
        let switch = unsafe { Pin::new_unchecked(&mut switch) };

        let raw_waker = RawWaker::new(
            ptr::null(),
            &RawWakerVTable::new(noop_clone, noop, noop, noop),
        );
        let waker = unsafe { Waker::from_raw(raw_waker) };
        let mut cx = Context::from_waker(&waker);

        assert!(switch.poll(&mut cx).is_ready());

        // The thread has been resumed.
        crate::rt::speculation::unwind_if_discarding();
    }

    pub(crate) fn spawn(stack_size: Option<usize>, f: Box<dyn FnOnce()>) {
        Self::with_state(|state| state.queued_spawn.push_back(QueuedSpawn { stack_size, f }));
    }

    pub(crate) fn run<F>(&mut self, execution: &mut Execution, stack_size: Option<usize>, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        // If the execution panics, the coroutines are dropped right here,
        // which unwinds whatever is still on their stacks.
        let mut threads = Vec::new();
        threads.push(self.spawn_thread(Box::new(f), stack_size));
        threads[0].1.resume();

        loop {
            if execution.threads.is_complete() {
                // Every thread has returned, so every coroutine is back
                // where it waits for a thread to run.
                self.idle.append(&mut threads);
                return;
            }

            let active = execution.threads.active_id();

            let mut queued_spawn = Self::tick(&mut threads[active.as_usize()].1, execution);

            while let Some(th) = queued_spawn.pop_front() {
                assert!(threads.len() < self.max_threads);

                let thread_id = threads.len();
                let QueuedSpawn {
                    f,
                    stack_size: thread_stack_size,
                } = th;

                threads.push(self.spawn_thread(f, thread_stack_size.or(stack_size)));
                threads[thread_id].1.resume();
            }
        }
    }

    /// Returns a coroutine that is about to run `f`, reusing an idle one
    /// with the same stack size if there is one.
    fn spawn_thread(
        &mut self,
        f: Box<dyn FnOnce()>,
        stack_size: Option<usize>,
    ) -> (Option<usize>, Thread) {
        let idle = self.idle.iter().rposition(|(size, _)| *size == stack_size);

        let thread = match idle {
            Some(idle) => {
                let (_, mut thread) = self.idle.swap_remove(idle);
                thread.set_para(Some(f));
                thread
            }
            None => spawn_thread(f, stack_size),
        };

        (stack_size, thread)
    }

    fn tick(thread: &mut Thread, execution: &mut Execution) -> VecDeque<QueuedSpawn> {
        let mut queued_spawn = VecDeque::new();
        let state = RefCell::new(State {
            execution,
            queued_spawn: &mut queued_spawn,
        });

        STATE.set(unsafe { transmute_lt(&state) }, || {
            thread.resume();
        });
        queued_spawn
    }

    fn with_state<F, R>(f: F) -> R
    where
        F: FnOnce(&mut State<'_>) -> R,
    {
        if !STATE.is_set() {
            panic!("cannot access Loom execution state from outside a Loom model. \
            are you accessing a Loom synchronization primitive from outside a Loom test (a call to `model` or `check`)?")
        }
        STATE.with(|state| {
            // When unwinding after a panic, `STATE` is unset before `Scheduler` is dropped.
            // However, `Scheduler::queued_spawn` could hold loom objects which would try to use
            // `STATE` when they are dropped. Because of that, we need to clear `queued_spawn`
            // while STATE is still set. Since `STATE` has an exclusive reference (&mut) to
            // `Scheduler::queued_spawn`, we also need to use `STATE` ourselves for accessing them,
            // but drop our `RefMut` before the dropping of `queued_spawn` happens.
            //
            // To implement this, we exploit the fact that the struct fields of `DropGuard`
            // are dropped in declaration order, and move `queued_spawn`in `DropGuard::drop`
            // to the second struct field of `DropGuard` (replacing it with an empty `VecDeque`).
            // Then, the destructor first drops the `RefMut` (thereby allowing `STATE` to be
            // borrowed again), and then the former `queued_spawn` value (possibly accessing `STATE`).
            struct DropGuard<'a, 'b>(std::cell::RefMut<'a, State<'b>>, VecDeque<QueuedSpawn>);
            impl<'a, 'b> Drop for DropGuard<'a, 'b> {
                fn drop(&mut self) {
                    // Unless the execution carries on after the panic: a
                    // panic held back while a load's guess is open, or an
                    // execution that is being dropped one thread at a time
                    // (see `rt::speculation`). The threads queued to be
                    // spawned are part of it then.
                    if std::thread::panicking()
                        && !crate::rt::speculation::carries_on(self.0.execution)
                    {
                        self.1 = std::mem::take(self.0.queued_spawn);
                    }
                }
            }
            impl<'a, 'b> DropGuard<'a, 'b> {
                fn run<F, R>(&mut self, f: F) -> R
                where
                    F: FnOnce(&mut State<'b>) -> R,
                {
                    f(&mut self.0)
                }
            }
            let mut guard = DropGuard(state.borrow_mut(), Default::default());
            guard.run(f)
        })
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Dropping the coroutines cancels them.
            return;
        }

        // Let the idle coroutines run to their end.
        for (_, thread) in &mut self.idle {
            thread.resume();
            assert!(thread.is_done());
        }
    }
}

fn spawn_thread(f: Box<dyn FnOnce()>, stack_size: Option<usize>) -> Thread {
    let body = move || {
        loop {
            let f: Option<Option<Box<dyn FnOnce()>>> = generator::yield_(());

            if let Some(f) = f {
                generator::yield_with(());
                f.unwrap()();
            } else {
                break;
            }
        }

        generator::done!();
    };
    let mut g = match stack_size {
        Some(stack_size) => Gn::new_opt(stack_size, body),
        None => Gn::new(body),
    };
    g.resume();
    g.set_para(Some(f));
    g
}

unsafe fn transmute_lt<'a, 'b>(state: &'a RefCell<State<'b>>) -> &'a RefCell<State<'static>> {
    ::std::mem::transmute(state)
}
