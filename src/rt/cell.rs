use crate::rt::location::{self, Location, LocationSet};
use crate::rt::{self, object, thread, Scheduler, VersionVec};

/// Tracks immutable and mutable access to a single memory cell.
#[derive(Debug)]
pub(crate) struct Cell {
    state: object::Ref<State>,
}

#[derive(Debug)]
pub(super) struct State {
    /// Where the cell was created
    created_location: Location,

    /// What its creator knew of when it was created.
    created_at: VersionVec,

    /// Number of threads currently reading the cell
    is_reading: usize,

    /// `true` if in a `with_mut` closure.
    is_writing: bool,

    /// The transitive closure of all immutable accesses of `data`.
    read_access: VersionVec,

    /// Location for the *last* time a thread read from the cell.
    read_locations: LocationSet,

    /// The last mutable access of `data`.
    write_access: VersionVec,

    /// Location for the *last* time a thread wrote to the cell
    write_locations: LocationSet,
}

#[derive(Debug)]
pub(crate) struct Reading {
    state: object::Ref<State>,

    /// What the access comes after by a dependency from a consume load, if
    /// the cell was got to through the pointer that was loaded: all of it,
    /// from start to end, and nothing else the thread does.
    dependency: Option<VersionVec>,
}

#[derive(Debug)]
pub(crate) struct Writing {
    state: object::Ref<State>,

    /// What the access comes after by a dependency from a consume load, if
    /// the cell was got to through the pointer that was loaded: all of it,
    /// from start to end, and nothing else the thread does.
    dependency: Option<VersionVec>,
}

impl Cell {
    pub(crate) fn new(location: Location) -> Cell {
        rt::execution(|execution| {
            let state = State::new(&execution.threads, location);

            Cell {
                state: execution.objects.insert(state),
            }
        })
    }

    pub(crate) fn start_read(&self, location: Location) -> Reading {
        // Enter the read closure
        rt::synchronize(|execution| {
            // `self` is inside the `UnsafeCell` of the program under test:
            // an access through a pointer from a consume load comes after
            // the store that the load read.
            let state = self.state.get_mut(&mut execution.objects);
            let dependency = execution
                .threads
                .active()
                .dependency(self as *const Self as usize, &state.created_at);

            assert!(!state.is_writing, "currently writing to cell");

            state.is_reading += 1;
            state.read_locations.track(location, &execution.threads);
            state.track_read(&execution.threads, &dependency);

            Reading {
                state: self.state,
                dependency,
            }
        })
    }

    pub(crate) fn start_write(&self, location: Location) -> Writing {
        // Enter the write closure
        rt::synchronize(|execution| {
            // `self` is inside the `UnsafeCell` of the program under test:
            // an access through a pointer from a consume load comes after
            // the store that the load read.
            let state = self.state.get_mut(&mut execution.objects);
            let dependency = execution
                .threads
                .active()
                .dependency(self as *const Self as usize, &state.created_at);

            assert!(state.is_reading == 0, "currently reading from cell");
            assert!(!state.is_writing, "currently writing to cell");

            state.is_writing = true;
            state.write_locations.track(location, &execution.threads);
            state.track_write(&execution.threads, &dependency);

            Writing {
                state: self.state,
                dependency,
            }
        })
    }
}

impl Drop for Cell {
    /// Destroying the cell invalidates every pointer handed out for it, so
    /// none may still be alive.
    ///
    /// This lives here, rather than on the generic `UnsafeCell<T>`, so that
    /// the mock does not get stricter drop-check rules than
    /// `std::cell::UnsafeCell`, which has no `Drop` impl.
    fn drop(&mut self) {
        // Don't turn an unwinding panic into an abort.
        if std::thread::panicking() {
            return;
        }

        // The cell may be dropped after the execution that tracks it is gone,
        // in which case there is nothing left to check against.
        let _ = Scheduler::try_with_execution(|execution| {
            let state = match self.state.try_get(&execution.objects) {
                Some(state) => state,
                None => return,
            };

            if state.is_reading > 0 || state.is_writing {
                let access = if state.is_writing {
                    "mutable"
                } else {
                    "immutable"
                };

                location::panic(format!(
                    "`UnsafeCell` dropped or consumed while {} access to it is still active.",
                    access
                ))
                .location("created", state.created_location)
                .fire();
            }
        });
    }
}

impl State {
    fn new(threads: &thread::Set, location: Location) -> State {
        let version = threads.active().causality;

        State {
            created_location: location,
            created_at: version,
            is_reading: 0,
            is_writing: false,
            read_access: version,
            read_locations: LocationSet::new(),
            write_access: version,
            write_locations: LocationSet::new(),
        }
    }

    /// What an access by a thread that knows of `current` comes after.
    fn after(current: &VersionVec, dependency: &Option<VersionVec>) -> VersionVec {
        let mut after = *current;

        if let Some(dependency) = dependency {
            after.join(dependency);
        }

        after
    }

    /// Perform a read access
    ///
    /// `dependency`: what the access comes after besides what the thread
    /// knows of, see `Reading`. That is not part of what later accesses
    /// have to come after.
    fn track_read(&mut self, threads: &thread::Set, dependency: &Option<VersionVec>) {
        let current = &threads.active().causality;
        let after = Self::after(current, dependency);

        // Check that there is no concurrent mutable access, i.e., the last
        // mutable access must happen-before this immutable access.
        if let Some(writer) = after.ahead(&self.write_access) {
            location::panic("Causality violation: Concurrent read and write accesses.")
                .location("created", self.created_location)
                .thread("read", threads.active_id(), self.read_locations[threads])
                .thread("write", writer, self.write_locations[writer])
                .fire();
        }

        self.read_access.join(current);
    }

    /// Perform a write access. `dependency`: see `track_read`.
    fn track_write(&mut self, threads: &thread::Set, dependency: &Option<VersionVec>) {
        let current = &threads.active().causality;
        let after = Self::after(current, dependency);

        // Check that there is no concurrent mutable access, i.e., the last
        // mutable access must happen-before this mutable access.
        if let Some(other) = after.ahead(&self.write_access) {
            location::panic("Causality violation: Concurrent write accesses to `UnsafeCell`.")
                .location("created", self.created_location)
                .thread("write one", other, self.write_locations[other])
                .thread(
                    "write two",
                    threads.active_id(),
                    self.write_locations[threads],
                )
                .fire();
        }

        // Check that there are no concurrent immutable accesses, i.e., every
        // immutable access must happen-before this mutable access.
        if let Some(reader) = after.ahead(&self.read_access) {
            location::panic(
                "Causality violation: Concurrent read and write accesses to `UnsafeCell`.",
            )
            .location("created", self.created_location)
            .thread("read", reader, self.read_locations[reader])
            .thread("write", threads.active_id(), self.write_locations[threads])
            .fire();
        }

        self.write_access.join(current);
    }
}

// === impl Reading ===

impl Drop for Reading {
    fn drop(&mut self) {
        rt::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);

            assert!(state.is_reading > 0);
            assert!(!state.is_writing);

            state.is_reading -= 1;

            if !std::thread::panicking() {
                // The end of the access is its own point in the thread's
                // history, after anything the thread published while the
                // access was open (see `Writing::drop`).
                execution.threads.active_causality_inc();
                state.track_read(&execution.threads, &self.dependency);
            }
        })
    }
}

// === impl Writing ===

impl Drop for Writing {
    fn drop(&mut self) {
        rt::execution(|execution| {
            let state = self.state.get_mut(&mut execution.objects);

            assert!(state.is_writing);
            assert!(state.is_reading == 0);

            state.is_writing = false;

            if !std::thread::panicking() {
                // The end of the access is its own point in the thread's
                // history. Without advancing the clock here, a `Release`
                // store made from inside the access as the thread's last
                // synchronizing operation would carry the same clock value as
                // the end of the access, and a thread acquiring that store
                // would appear to have observed the whole access, when in
                // fact the access was still open.
                execution.threads.active_causality_inc();
                state.track_write(&execution.threads, &self.dependency);
            }
        })
    }
}
