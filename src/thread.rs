//! Mock implementation of `std::thread`.

pub use crate::rt::thread::AccessError;
pub use crate::rt::yield_now;
use crate::rt::{self, Execution, Location};

#[doc(no_inline)]
pub use std::thread::panicking;

use std::cell::RefCell;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::{fmt, io};

use tracing::trace;

/// Mock implementation of `std::thread::JoinHandle`.
pub struct JoinHandle<T> {
    result: Arc<Mutex<Option<std::thread::Result<T>>>>,
    notify: rt::Notify,
    thread: Thread,
}

/// Mock implementation of `std::thread::Thread`.
#[derive(Clone, Debug)]
pub struct Thread {
    id: ThreadId,
    name: Option<String>,
}

impl Thread {
    /// Returns a unique identifier for this thread
    pub fn id(&self) -> ThreadId {
        self.id
    }

    /// Returns the (optional) name of this thread
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Mock implementation of [`std::thread::Thread::unpark`].
    ///
    /// Atomically makes the handle's token available if it is not already.
    ///
    /// Every thread is equipped with some basic low-level blocking support, via
    /// the [`park`] function and the `unpark()` method. These can be
    /// used as a more CPU-efficient implementation of a spinlock.
    ///
    /// See the [park documentation][park] for more details.
    pub fn unpark(&self) {
        rt::execution(|execution| execution.threads.unpark(self.id.id));
    }
}

/// Mock implementation of `std::thread::ThreadId`.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct ThreadId {
    id: crate::rt::thread::Id,
}

impl std::fmt::Debug for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ThreadId({})", self.id.public_id())
    }
}

/// Mock implementation of `std::thread::LocalKey`.
pub struct LocalKey<T> {
    // Sadly, these fields have to be public, since function pointers in const
    // fns are unstable. When fn pointer arguments to const fns stabilize, these
    // should be made private and replaced with a `const fn new`.
    //
    // User code should not rely on the existence of these fields.
    #[doc(hidden)]
    pub init: fn() -> T,
    #[doc(hidden)]
    pub _p: PhantomData<fn(T)>,
}

/// Thread factory, which can be used in order to configure the properties of
/// a new thread.
#[derive(Debug)]
pub struct Builder {
    name: Option<String>,
    stack_size: Option<usize>,
}

static CURRENT_THREAD_KEY: LocalKey<Thread> = LocalKey {
    init: || unreachable!(),
    _p: PhantomData,
};

fn init_current(execution: &mut Execution, name: Option<String>) -> Thread {
    let id = execution.threads.active_id();
    let thread = Thread {
        id: ThreadId { id },
        name,
    };

    execution
        .threads
        .local_init(&CURRENT_THREAD_KEY, thread.clone());

    thread
}

/// Returns a handle to the current thread.
pub fn current() -> Thread {
    rt::execution(|execution| {
        let thread = execution.threads.local(&CURRENT_THREAD_KEY);
        if let Some(thread) = thread {
            thread.unwrap().clone()
        } else {
            // Lazily initialize the current() Thread. This is done to help
            // handle the initial (unnamed) bootstrap thread.
            init_current(execution, None)
        }
    })
}

/// Mock implementation of `std::thread::spawn`.
///
/// Note that you may only have [`MAX_THREADS`](crate::MAX_THREADS) threads in a given loom tests
/// _including_ the main thread.
#[track_caller]
pub fn spawn<F, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T,
    F: 'static,
    T: 'static,
{
    // Safety: everything is `'static`, there is nothing to outlive.
    unsafe { spawn_internal(f, None, None, None, location!()) }
}

/// Mock implementation of `std::thread::park`.
///
///  Blocks unless or until the current thread's token is made available.
///
/// A call to `park` does not guarantee that the thread will remain parked
/// forever, and callers should be prepared for this possibility.
#[track_caller]
pub fn park() {
    rt::park(location!());
}

/// Spawns a thread whose closure and result may borrow from the caller.
///
/// # Safety
///
/// As for `std::thread::Builder::spawn_unchecked`: the caller must make sure
/// the spawned thread does not outlive anything `f` or its result borrow, by
/// waiting for the thread before those borrows end. Waiting on `scope_done`,
/// if given, is sufficient: it is notified once the thread no longer holds
/// on to `f` or to its result.
unsafe fn spawn_internal<'a, F, T>(
    f: F,
    name: Option<String>,
    stack_size: Option<usize>,
    scope_done: Option<rt::Notify>,
    location: Location,
) -> JoinHandle<T>
where
    F: FnOnce() -> T,
    F: 'a,
    T: 'a,
{
    let result = Arc::new(Mutex::new(None));
    let notify = rt::Notify::new(true, false);

    let body: Box<dyn FnOnce() + 'a> = {
        let name = name.clone();
        let result = result.clone();
        Box::new(move || {
            rt::execution(|execution| {
                init_current(execution, name);
            });

            *result.lock().unwrap() = Some(Ok(f()));
            notify.notify(location);

            // Let go of the result slot before telling a scope that the
            // thread is finished: if the join handle is already gone, this
            // is where the result gets dropped, and that has to happen
            // before the scope lets its borrows end.
            drop(result);

            if let Some(scope_done) = scope_done {
                scope_done.notify(location);
            }
        })
    };

    // Erase the lifetime, which the caller is responsible for upholding.
    let body = std::mem::transmute::<Box<dyn FnOnce() + 'a>, Box<dyn FnOnce() + 'static>>(body);

    let id = rt::spawn(stack_size, body);

    JoinHandle {
        result,
        notify,
        thread: Thread {
            id: ThreadId { id },
            name,
        },
    }
}

impl Builder {
    /// Generates the base configuration for spawning a thread, from which
    /// configuration methods can be chained.
    // `std::thread::Builder` does not implement `Default`, so this type does
    // not either, as it's a mock version of the `std` type.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Builder {
        Builder {
            name: None,
            stack_size: None,
        }
    }

    /// Names the thread-to-be. Currently the name is used for identification
    /// only in panic messages.
    pub fn name(mut self, name: String) -> Builder {
        self.name = Some(name);

        self
    }

    /// Sets the size of the stack for the new thread.
    ///
    /// Unlike `std`, the size is not in bytes: it is handed to the underlying
    /// coroutine library as is, which measures it in machine words (`usize`).
    ///
    /// Threads that do not set a size use
    /// [`model::Builder::stack_size`](crate::model::Builder::stack_size).
    pub fn stack_size(mut self, size: usize) -> Builder {
        self.stack_size = Some(size);

        self
    }

    /// Spawns a new thread by taking ownership of the `Builder`, and returns an
    /// `io::Result` to its `JoinHandle`.
    #[track_caller]
    pub fn spawn<F, T>(self, f: F) -> io::Result<JoinHandle<T>>
    where
        F: FnOnce() -> T,
        F: Send + 'static,
        T: Send + 'static,
    {
        // Safety: everything is `'static`, there is nothing to outlive.
        Ok(unsafe { spawn_internal(f, self.name, self.stack_size, None, location!()) })
    }

    /// Spawns a new scoped thread using the settings set through this
    /// `Builder`.
    ///
    /// Mock implementation of `std::thread::Builder::spawn_scoped`.
    #[track_caller]
    pub fn spawn_scoped<'scope, 'env, F, T>(
        self,
        scope: &'scope Scope<'scope, 'env>,
        f: F,
    ) -> io::Result<ScopedJoinHandle<'scope, T>>
    where
        F: FnOnce() -> T + Send + 'scope,
        T: Send + 'scope,
    {
        let done = rt::Notify::new(true, false);
        scope.pending.lock().unwrap().push(done);

        // Safety: a `&'scope Scope` only exists inside a call to `scope()`,
        // which does not return (ending `'scope`) before it has waited for
        // `done`, registered above.
        let handle =
            unsafe { spawn_internal(f, self.name, self.stack_size, Some(done), location!()) };

        Ok(ScopedJoinHandle {
            handle,
            _scope: PhantomData,
        })
    }
}

impl<T> JoinHandle<T> {
    /// Waits for the associated thread to finish.
    ///
    /// Unlike `std`, this never returns `Err`: a panic on a modeled thread
    /// fails the whole model immediately instead of being delivered here.
    /// See the crate documentation on
    /// [panics in threads](crate#panics-in-threads).
    #[track_caller]
    pub fn join(self) -> std::thread::Result<T> {
        self.notify.wait(location!());
        self.result.lock().unwrap().take().unwrap()
    }

    /// Checks if the associated thread has finished running its main function.
    ///
    /// Mock implementation of [`std::thread::JoinHandle::is_finished`]. This is
    /// a modeled operation: loom explores executions in which the thread has
    /// and has not yet finished at the time of the call.
    #[track_caller]
    pub fn is_finished(&self) -> bool {
        self.notify.is_notified(location!())
    }

    /// Gets a handle to the underlying [`Thread`]
    pub fn thread(&self) -> &Thread {
        &self.thread
    }
}

/// A scope to spawn scoped threads in.
///
/// Mock implementation of `std::thread::Scope`. See [`scope`] for details.
pub struct Scope<'scope, 'env: 'scope> {
    /// Notified by each thread spawned in the scope when it has finished;
    /// holds the ones `scope()` has not waited for yet.
    pending: Mutex<Vec<rt::Notify>>,
    // Invariance over both lifetimes, as in `std`.
    _scope: PhantomData<&'scope mut &'scope ()>,
    _env: PhantomData<&'env mut &'env ()>,
}

/// An owned permission to join on a scoped thread (block on its termination).
///
/// Mock implementation of `std::thread::ScopedJoinHandle`. See
/// [`Scope::spawn`] for details.
pub struct ScopedJoinHandle<'scope, T> {
    handle: JoinHandle<T>,
    _scope: PhantomData<&'scope ()>,
}

/// Creates a scope for spawning scoped threads.
///
/// Mock implementation of `std::thread::scope`.
///
/// Threads spawned through the [`Scope`] passed to `f` may borrow non-`'static`
/// data from outside the scope. All of them are waited for before this
/// function returns, whether or not they were joined explicitly.
///
/// As everywhere in loom, a panic on any thread fails the model rather than
/// being reported through `join`, so unlike `std` this never has to turn a
/// panicked scoped thread into a panic of its own.
#[track_caller]
pub fn scope<'env, F, T>(f: F) -> T
where
    F: for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> T,
{
    let location = location!();

    let scope = Scope {
        pending: Mutex::new(Vec::new()),
        _scope: PhantomData,
        _env: PhantomData,
    };

    // Run `f`, but catch panics so that the scoped threads are waited for in
    // that case as well.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&scope)));

    // Wait for every thread spawned in the scope. A scoped thread can itself
    // spawn into the scope, so keep going until nothing is left: a thread
    // registers the threads it spawns before it reports itself as finished.
    loop {
        let next = scope.pending.lock().unwrap().pop();

        match next {
            Some(done) => done.wait(location),
            None => break,
        }
    }

    match result {
        Ok(value) => value,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

impl<'scope, 'env> Scope<'scope, 'env> {
    /// Spawns a new thread within a scope, returning a [`ScopedJoinHandle`]
    /// for it.
    ///
    /// Mock implementation of `std::thread::Scope::spawn`.
    #[track_caller]
    pub fn spawn<F, T>(&'scope self, f: F) -> ScopedJoinHandle<'scope, T>
    where
        F: FnOnce() -> T + Send + 'scope,
        T: Send + 'scope,
    {
        Builder::new()
            .spawn_scoped(self, f)
            .expect("failed to spawn thread")
    }
}

impl fmt::Debug for Scope<'_, '_> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Scope").finish_non_exhaustive()
    }
}

impl<T> ScopedJoinHandle<'_, T> {
    /// Extracts a handle to the underlying thread.
    pub fn thread(&self) -> &Thread {
        self.handle.thread()
    }

    /// Waits for the associated thread to finish.
    #[track_caller]
    pub fn join(self) -> std::thread::Result<T> {
        self.handle.join()
    }

    /// Checks if the associated thread has finished running its main function.
    #[track_caller]
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
}

impl<T> fmt::Debug for ScopedJoinHandle<'_, T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("ScopedJoinHandle").finish_non_exhaustive()
    }
}

impl<T: fmt::Debug> fmt::Debug for JoinHandle<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("JoinHandle").finish()
    }
}

fn _assert_traits() {
    fn assert<T: Send + Sync>() {}

    assert::<JoinHandle<()>>();
    assert::<ScopedJoinHandle<'_, ()>>();
    assert::<Scope<'_, '_>>();
}

impl<T: 'static> LocalKey<T> {
    /// Mock implementation of `std::thread::LocalKey::with`.
    pub fn with<F, R>(&'static self, f: F) -> R
    where
        F: FnOnce(&T) -> R,
    {
        self.try_with(f)
            .expect("cannot access a (mock) TLS value during or after it is destroyed")
    }

    /// Mock implementation of `std::thread::LocalKey::try_with`.
    pub fn try_with<F, R>(&'static self, f: F) -> Result<R, AccessError>
    where
        F: FnOnce(&T) -> R,
    {
        let value = match unsafe { self.get_inner() } {
            Some(v) => v?,
            None => {
                // Init the value out of the `rt::execution`
                let value = (self.init)();

                rt::execution(|execution| {
                    trace!("LocalKey::try_with");

                    execution.threads.local_init(self, value);
                });

                unsafe { self.get_inner() }.expect("bug")?
            }
        };
        Ok(f(value))
    }

    unsafe fn get_inner(&'static self) -> Option<Result<&'static T, AccessError>> {
        unsafe fn transmute_lt<'a, 'b, T>(t: &'a T) -> &'b T {
            std::mem::transmute::<&'a T, &'b T>(t)
        }

        rt::execution(|execution| {
            trace!("LocalKey::get_inner");

            let res = execution.threads.local(self)?;

            let local = match res {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };

            // This is, sadly, necessary to allow nested `with` blocks to access
            // different thread locals. The borrow on the thread-local needs to
            // escape the lifetime of the borrow on `execution`, since
            // `rt::execution` mutably borrows a RefCell, and borrowing it twice will
            // cause a panic. This should be safe, as we know the function being
            // passed the thread local will not outlive the thread on which
            // it's executing, by construction --- it's just kind of unfortunate.
            Some(Ok(transmute_lt(local)))
        })
    }
}

impl<T: 'static> LocalKey<T> {
    /// Runs `f` with the thread local, initializing it with `value` rather
    /// than the key's lazy initializer if it has not been initialized yet.
    ///
    /// `f` is handed `value` back if the key was already initialized.
    fn with_init_value<F, R>(&'static self, value: T, f: F) -> R
    where
        F: FnOnce(Option<T>, &T) -> R,
    {
        const DESTROYED: &str = "cannot access a (mock) TLS value during or after it is destroyed";

        match unsafe { self.get_inner() } {
            Some(current) => f(Some(value), current.expect(DESTROYED)),
            None => {
                rt::execution(|execution| {
                    trace!("LocalKey::with_init_value");

                    execution.threads.local_init(self, value);
                });

                f(
                    None,
                    unsafe { self.get_inner() }.expect("bug").expect(DESTROYED),
                )
            }
        }
    }
}

macro_rules! local_key_cell {
    ($cell:ty, $name:literal) => {
        #[doc = concat!("Convenience methods for thread locals holding a [`", $name, "`].")]
        impl<T: 'static> LocalKey<$cell> {
            /// Mock implementation of `std::thread::LocalKey::<Cell<T>>::set`.
            ///
            /// Sets the contained value. Like `std`, this does not run the
            /// lazy initializer if the key has not been initialized yet.
            #[track_caller]
            pub fn set(&'static self, value: T) {
                self.with_init_value(<$cell>::new(value), |value, cell| {
                    if let Some(value) = value {
                        cell.set(value.into_inner());
                    }
                })
            }

            /// Mock implementation of `std::thread::LocalKey::<Cell<T>>::get`.
            #[track_caller]
            pub fn get(&'static self) -> T
            where
                T: Copy,
            {
                self.with(|cell| cell.get())
            }

            /// Mock implementation of `std::thread::LocalKey::<Cell<T>>::take`.
            #[track_caller]
            pub fn take(&'static self) -> T
            where
                T: Default,
            {
                self.with(|cell| cell.take())
            }

            /// Mock implementation of
            /// `std::thread::LocalKey::<Cell<T>>::replace`.
            #[track_caller]
            pub fn replace(&'static self, value: T) -> T {
                self.with(|cell| cell.replace(value))
            }
        }
    };
}

local_key_cell!(std::cell::Cell<T>, "std::cell::Cell");
local_key_cell!(crate::cell::Cell<T>, "crate::cell::Cell");

impl<T: 'static> LocalKey<RefCell<T>> {
    /// Mock implementation of
    /// `std::thread::LocalKey::<RefCell<T>>::with_borrow`.
    pub fn with_borrow<F, R>(&'static self, f: F) -> R
    where
        F: FnOnce(&T) -> R,
    {
        self.with(|cell| f(&cell.borrow()))
    }

    /// Mock implementation of
    /// `std::thread::LocalKey::<RefCell<T>>::with_borrow_mut`.
    pub fn with_borrow_mut<F, R>(&'static self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        self.with(|cell| f(&mut cell.borrow_mut()))
    }

    /// Mock implementation of `std::thread::LocalKey::<RefCell<T>>::set`.
    ///
    /// Sets the contained value. Like `std`, this does not run the lazy
    /// initializer if the key has not been initialized yet.
    pub fn set(&'static self, value: T) {
        self.with_init_value(RefCell::new(value), |value, cell| {
            if let Some(value) = value {
                *cell.borrow_mut() = value.into_inner();
            }
        })
    }

    /// Mock implementation of `std::thread::LocalKey::<RefCell<T>>::take`.
    pub fn take(&'static self) -> T
    where
        T: Default,
    {
        self.with(RefCell::take)
    }

    /// Mock implementation of `std::thread::LocalKey::<RefCell<T>>::replace`.
    pub fn replace(&'static self, value: T) -> T {
        self.with(|cell| cell.replace(value))
    }
}

impl<T: 'static> fmt::Debug for LocalKey<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("LocalKey { .. }")
    }
}
