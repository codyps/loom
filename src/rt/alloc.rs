use crate::rt;
use crate::rt::{location, object, Location, Scheduler};

use tracing::trace;

/// Tracks an allocation
#[derive(Debug)]
pub(crate) struct Allocation {
    state: object::Ref<State>,
}

#[derive(Debug)]
pub(super) struct State {
    is_dropped: bool,
    allocated: Location,
    /// Where the allocation was freed, once it has been.
    freed: Location,
}

/// Track a raw allocation
pub(crate) fn alloc(ptr: *mut u8, location: Location) {
    let mut prev = None;

    rt::execution(|execution| {
        let state = execution.objects.insert(State {
            is_dropped: false,
            allocated: location,
            freed: Location::disabled(),
        });

        let allocation = Allocation { state };

        trace!(?allocation.state, ?ptr, %location, "alloc");

        prev = execution.raw_allocations.insert(ptr as usize, allocation);

        // The tracker of a freed allocation stays behind to recognize a
        // double free, until the allocator hands the address out again.
        if let Some(prev) = &prev {
            assert!(
                prev.state.get(&execution.objects).is_dropped,
                "pointer already tracked"
            );
        }
    });

    // Drop outside of the `rt::execution` block
    drop(prev);
}

/// Track a raw deallocation
pub(crate) fn dealloc(ptr: *mut u8, location: Location) {
    rt::execution(|execution| {
        let allocation = match execution.raw_allocations.get(&(ptr as usize)) {
            Some(allocation) => allocation,
            None => panic!(
                "Tried to free a pointer that loom is not tracking. It was not \
                 allocated with `loom::alloc`."
            ),
        };

        let state = allocation.state.get_mut(&mut execution.objects);

        trace!(state = ?allocation.state, ?ptr, %location, "dealloc");

        if state.is_dropped {
            location::panic("Double free: the allocation has already been freed.")
                .location("allocated", state.allocated)
                .location("first freed", state.freed)
                .location("freed again", location)
                .fire();
        }

        // The tracker is left in place, see `alloc`.
        state.is_dropped = true;
        state.freed = location;
    });
}

impl Allocation {
    pub(crate) fn new(location: Location) -> Allocation {
        rt::execution(|execution| {
            let state = execution.objects.insert(State {
                is_dropped: false,
                allocated: location,
                freed: Location::disabled(),
            });

            trace!(?state, %location, "Allocation::new");

            Allocation { state }
        })
    }
}

impl Drop for Allocation {
    #[track_caller]
    fn drop(&mut self) {
        let caller = std::panic::Location::caller();

        // The tracker may be dropped when there is no execution to report to
        // anymore. In particular, the trackers of raw allocations that were
        // leaked are owned by the execution itself and are dropped along with
        // it, which happens while unwinding from the "leaked" panic: failing
        // here would abort the process and hide that report.
        let _ = Scheduler::try_with_execution(|execution| {
            let location = if execution.location {
                Location::from(caller)
            } else {
                Location::disabled()
            };

            if let Some(state) = self.state.try_get_mut(&mut execution.objects) {
                trace!(state = ?self.state, drop.location = %location, "Allocation::drop");

                if !state.is_dropped {
                    state.is_dropped = true;
                    state.freed = location;
                }
            }
        });
    }
}

impl State {
    pub(super) fn check_for_leaks(&self, index: usize) {
        if !self.is_dropped {
            if self.allocated.is_captured() {
                panic!(
                    "Allocation leaked.\n  Allocated: {}\n      Index: {}",
                    self.allocated, index
                );
            } else {
                panic!("Allocation leaked.\n  Index: {}", index);
            }
        }
    }
}
