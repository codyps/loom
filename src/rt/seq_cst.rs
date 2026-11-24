//! The order of `SeqCst` operations.
//!
//! All `SeqCst` loads, stores and fences of a program are part of a single
//! total order, which every thread agrees on and which has to be consistent
//! with what the operations observe. Whether such an order exists is what
//! distinguishes `SeqCst` from `AcqRel`.
//!
//! Loom does not pick that order. It only records which operations are
//! required to come before which others, as edges of a graph, and makes sure
//! the graph stays free of cycles: any order of the operations that respects
//! the edges will do. This follows the `psc` relation of "Repairing
//! Sequential Consistency in C/C++11" (Lahav, Vafeiadis, Kang, Hur, Dreyer;
//! PLDI 2017), which is the formal counterpart of the rules in C++20:
//!
//! - Operations of one thread are ordered as they are executed.
//! - Happens-before orders operations of different threads, when it relates
//!   two accesses of the same atomic, or when each of the two has a
//!   neighbour in its thread that accesses a different location and the
//!   neighbours are ordered. (Happens-before between two operations that is
//!   established by nothing but those two operations does not count. That is
//!   the weakening RC11 and C++20 made so that `SeqCst` can be compiled
//!   efficiently, and it is observable.)
//! - On one atomic, an operation comes before a store that is later in the
//!   modification order: an earlier store does, and so does a load of an
//!   earlier store.
//! - Fences are ordered like accesses, with happens-before extending them on
//!   both sides: what happens after a fence counts as the fence when it is
//!   the earlier operation, and what happens before a fence counts as the
//!   fence when it is the later one.
//!
//! The first two kinds of edges are known when an operation executes. The
//! others depend on the modification order, which loom decides lazily, so
//! they show up whenever a load settles the order of two stores. A load may
//! only return a store if the edges that follow from it do not close a
//! cycle.

use crate::rt::{VersionVec, MAX_THREADS};

/// A set of nodes of the graph.
///
/// These are created, copied and merged for every atomic operation, and
/// nearly always hold only a few low-numbered nodes: the first 64 are kept
/// in place, without an allocation.
#[derive(Debug, Clone, Default)]
pub(crate) struct NodeSet {
    /// Nodes 0 to 63.
    first: u64,

    /// Nodes from 64 on.
    rest: Vec<u64>,
}

impl NodeSet {
    pub(crate) fn insert(&mut self, node: usize) {
        if node < 64 {
            self.first |= 1 << node;
            return;
        }

        let word = node / 64 - 1;

        if self.rest.len() <= word {
            self.rest.resize(word + 1, 0);
        }

        self.rest[word] |= 1 << (node % 64);
    }

    pub(crate) fn contains(&self, node: usize) -> bool {
        if node < 64 {
            return self.first & (1 << node) != 0;
        }

        self.rest
            .get(node / 64 - 1)
            .map_or(false, |word| word & (1 << (node % 64)) != 0)
    }

    pub(crate) fn union_with(&mut self, other: &NodeSet) {
        self.first |= other.first;

        if self.rest.len() < other.rest.len() {
            self.rest.resize(other.rest.len(), 0);
        }

        for (word, other) in self.rest.iter_mut().zip(&other.rest) {
            *word |= other;
        }
    }

    /// Returns `true` if every node of `other` is in this set, not counting
    /// `except`.
    pub(crate) fn contains_all(&self, other: &NodeSet, except: usize) -> bool {
        let has_all = |i: usize, other: u64| {
            let own = match i {
                0 => self.first,
                _ => self.rest.get(i - 1).copied().unwrap_or(0),
            };

            let mut missing = other & !own;

            if except / 64 == i {
                missing &= !(1 << (except % 64));
            }

            missing == 0
        };

        has_all(0, other.first)
            && other
                .rest
                .iter()
                .enumerate()
                .all(|(i, &word)| has_all(i + 1, word))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.first == 0 && self.rest.iter().all(|word| *word == 0)
    }

    /// Keeps the nodes for which `keep` returns `true`.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(usize) -> bool) {
        let nodes: Vec<usize> = self.iter().collect();

        for node in nodes {
            if !keep(node) {
                if node < 64 {
                    self.first &= !(1 << node);
                } else {
                    self.rest[node / 64 - 1] &= !(1 << (node % 64));
                }
            }
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        std::iter::once(self.first)
            .chain(self.rest.iter().copied())
            .enumerate()
            .flat_map(|(i, mut word)| {
                std::iter::from_fn(move || {
                    if word == 0 {
                        return None;
                    }

                    let bit = word.trailing_zeros() as usize;
                    word &= word - 1;

                    Some(i * 64 + bit)
                })
            })
    }
}

/// Location of a fence, which is not an access to any atomic.
pub(crate) const NO_LOCATION: usize = usize::MAX;

/// Stands for whatever a thread does that is not an atomic operation.
pub(crate) const OTHER_LOCATION: usize = usize::MAX - 1;

#[derive(Debug, Clone)]
struct Node {
    thread: usize,

    /// The thread's own clock value when it executed the operation.
    clock: u16,

    /// The atomic that is accessed, or `NO_LOCATION` for a fence.
    location: usize,

    /// The clock value of the thread's first later operation on a different
    /// location, once there is one.
    next_elsewhere: Option<u16>,
}

/// A candidate for a new node: the operation the current thread is about to
/// execute.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Candidate<'a> {
    pub(crate) thread: usize,
    pub(crate) clock: u16,
    pub(crate) location: usize,

    /// What the thread knows of once the operation has executed, including
    /// what the operation itself synchronizes with.
    pub(crate) causality: &'a VersionVec,

    /// What the thread knew of at its last operation on a different
    /// location.
    pub(crate) elsewhere: &'a VersionVec,
}

#[derive(Debug, Default)]
pub(crate) struct SeqCst {
    nodes: Vec<Node>,

    /// For each node, the nodes that have to come after it, transitively.
    after: Vec<NodeSet>,

    /// The last node of each thread.
    last: [Option<usize>; MAX_THREADS],

    /// The nodes of each thread that are still waiting for the thread to
    /// execute an operation on a different location.
    waiting: [Vec<usize>; MAX_THREADS],
}

impl SeqCst {
    pub(crate) fn clear(&mut self) {
        self.nodes.clear();
        self.after.clear();
        self.last = Default::default();

        for waiting in &mut self.waiting {
            waiting.clear();
        }
    }

    /// Returns `true` as long as the program has not executed any `SeqCst`
    /// operation, in which case there is nothing to keep track of.
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The number of nodes, which is also the id the next node will get.
    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    /// `thread` executes an operation, of any ordering, on `location`.
    pub(crate) fn observe(&mut self, thread: usize, location: usize, clock: u16) {
        let nodes = &mut self.nodes;

        self.waiting[thread].retain(|&node| {
            if nodes[node].location == location && location != NO_LOCATION {
                return true;
            }

            nodes[node].next_elsewhere = Some(clock);
            false
        });
    }

    /// The fences that happen before a point at which a thread knows of
    /// `causality`.
    pub(crate) fn fences_before(&self, thread: usize, causality: &VersionVec) -> NodeSet {
        let mut fences = NodeSet::default();

        for (id, node) in self.nodes.iter().enumerate() {
            if node.location == NO_LOCATION
                && (node.thread == thread || causality.get(node.thread) >= node.clock)
            {
                fences.insert(id);
            }
        }

        fences
    }

    /// The nodes that come before `candidate` because of when they executed:
    /// the thread's previous node, and nodes of other threads that
    /// happens-before orders before it.
    fn predecessors(&self, candidate: &Candidate<'_>) -> NodeSet {
        let mut preds = NodeSet::default();

        if let Some(last) = self.last[candidate.thread] {
            preds.insert(last);
        }

        for (id, node) in self.nodes.iter().enumerate() {
            if node.thread == candidate.thread {
                continue;
            }

            let known = candidate.causality.get(node.thread) >= node.clock;

            let ordered = if candidate.location == NO_LOCATION || node.location == NO_LOCATION {
                // Happens-before extends a fence on either side.
                known
            } else {
                // Two accesses of the same atomic, or ...
                let same = node.location == candidate.location && known;

                // ... each with a neighbour that accesses something else.
                let elsewhere = node
                    .next_elsewhere
                    .map_or(false, |clock| candidate.elsewhere.get(node.thread) >= clock);

                same || elsewhere
            };

            if ordered {
                preds.insert(id);
            }
        }

        preds
    }

    /// Adds a node for the operation described by `candidate`.
    pub(crate) fn add(&mut self, candidate: &Candidate<'_>) -> usize {
        let preds = self.predecessors(candidate);
        let id = self.nodes.len();

        self.nodes.push(Node {
            thread: candidate.thread,
            clock: candidate.clock,
            location: candidate.location,
            next_elsewhere: None,
        });
        self.after.push(NodeSet::default());

        for pred in preds.iter() {
            let added = self.add_edge(pred, id);
            debug_assert!(added, "[loom internal bug] new node closes a cycle");
        }

        self.last[candidate.thread] = Some(id);
        self.waiting[candidate.thread].push(id);

        id
    }

    /// Returns `true` if `from` has to come before `to`.
    pub(crate) fn precedes(&self, from: usize, to: usize) -> bool {
        self.after
            .get(from)
            .map_or(false, |after| after.contains(to))
    }

    /// Requires `from` to come before `to`. Returns `false`, leaving the
    /// graph as it was, if `to` already has to come before `from`.
    pub(crate) fn add_edge(&mut self, from: usize, to: usize) -> bool {
        add_edge(&mut self.after, from, to)
    }

    /// Requires every node of `from` to come before every node of `to`,
    /// other than itself: a read-modify-write is both a load of one store
    /// and the next store, and is not ordered with respect to itself.
    ///
    /// Returns `false` if one of the nodes of `to` already has to come
    /// before one of `from`. Those two are left as they were, and the others
    /// are ordered all the same.
    pub(crate) fn add_edges(&mut self, from: &NodeSet, to: &NodeSet) -> bool {
        add_edges(&mut self.after, from, to)
    }

    /// Returns `true` if all of `edges` can be added together, along with a
    /// new node for `candidate` if there is one. The id of that node is
    /// `self.len()`, and `edges` may refer to it.
    pub(crate) fn admits(
        &self,
        candidate: Option<&Candidate<'_>>,
        edges: &[(NodeSet, NodeSet)],
    ) -> bool {
        // A new node on its own only gets edges leading to it. Without one,
        // edges that are all there already need no second look, which is
        // what nearly every call comes to.
        if candidate.is_none()
            && edges
                .iter()
                .all(|(from, to)| has_edges(&self.after, from, to))
        {
            return true;
        }

        let mut after = self.after.clone();

        if let Some(candidate) = candidate {
            let id = after.len();
            after.push(NodeSet::default());

            for pred in self.predecessors(candidate).iter() {
                if !add_edge(&mut after, pred, id) {
                    return false;
                }
            }
        }

        edges
            .iter()
            .all(|(from, to)| add_edges(&mut after, from, to))
    }
}

/// Returns `true` if every edge of `add_edges(after, from, to)` is there.
fn has_edges(after: &[NodeSet], from: &NodeSet, to: &NodeSet) -> bool {
    from.iter().all(|node| after[node].contains_all(to, node))
}

/// See `SeqCst::add_edges`.
fn add_edges(after: &mut [NodeSet], from: &NodeSet, to: &NodeSet) -> bool {
    let mut added = true;

    for from in from.iter() {
        // Nearly always the case: the edges of an atomic are asked for
        // again whenever it is accessed.
        if after[from].contains_all(to, from) {
            continue;
        }

        for to in to.iter() {
            if from != to {
                added &= add_edge(after, from, to);
            }
        }
    }

    added
}

fn add_edge(after: &mut [NodeSet], from: usize, to: usize) -> bool {
    if from == to || after[to].contains(from) {
        return false;
    }

    if after[from].contains(to) {
        return true;
    }

    let mut reached = after[to].clone();
    reached.insert(to);

    for node in 0..after.len() {
        if node == from || after[node].contains(from) {
            after[node].union_with(&reached);
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(nodes: usize) -> Vec<NodeSet> {
        vec![NodeSet::default(); nodes]
    }

    #[test]
    fn node_set() {
        let mut set = NodeSet::default();
        assert!(set.is_empty());
        assert!(!set.contains(3));

        set.insert(3);
        set.insert(130);
        assert!(set.contains(3) && set.contains(130));
        assert!(!set.contains(4) && !set.contains(1000));
        assert_eq!(set.iter().collect::<Vec<_>>(), [3, 130]);

        let mut other = NodeSet::default();
        other.insert(64);
        other.union_with(&set);
        assert_eq!(other.iter().collect::<Vec<_>>(), [3, 64, 130]);
    }

    #[test]
    fn edges_are_transitive() {
        let mut after = graph(4);

        assert!(add_edge(&mut after, 0, 1));
        assert!(add_edge(&mut after, 2, 3));
        assert!(add_edge(&mut after, 1, 2));

        // 0 -> 1 -> 2 -> 3
        assert!(after[0].contains(3));
        assert!(after[1].contains(3));
        assert!(!after[3].contains(0));

        // Adding what is already implied changes nothing.
        assert!(add_edge(&mut after, 0, 3));
    }

    #[test]
    fn cycles_are_refused() {
        let mut after = graph(3);

        assert!(add_edge(&mut after, 0, 1));
        assert!(add_edge(&mut after, 1, 2));

        assert!(!add_edge(&mut after, 2, 0));
        assert!(!add_edge(&mut after, 1, 1));

        // The refused edges left no trace.
        assert!(!after[2].contains(0));
        assert!(add_edge(&mut after, 0, 2));
    }

    #[test]
    fn admits_checks_edges_together() {
        let mut order = SeqCst::default();
        let view = VersionVec::new();

        for (thread, clock) in [(0, 1), (1, 1), (2, 1)] {
            order.add(&Candidate {
                thread,
                clock,
                location: thread,
                causality: &view,
                elsewhere: &view,
            });
        }

        assert_eq!(order.len(), 3);

        // Each edge is fine on its own, but not all three.
        let edge = |from, to| {
            let mut edge = (NodeSet::default(), NodeSet::default());
            edge.0.insert(from);
            edge.1.insert(to);
            edge
        };

        assert!(order.admits(None, &[edge(0, 1), edge(1, 2)]));
        assert!(!order.admits(None, &[edge(0, 1), edge(1, 2), edge(2, 0)]));

        // Checking does not add anything.
        assert!(order.add_edge(2, 0));
    }

    #[test]
    fn operations_of_a_thread_are_ordered() {
        let mut order = SeqCst::default();
        let view = VersionVec::new();

        let candidate = |clock| Candidate {
            thread: 0,
            clock,
            location: 7,
            causality: &view,
            elsewhere: &view,
        };

        let first = order.add(&candidate(1));
        let second = order.add(&candidate(2));

        assert!(!order.add_edge(second, first));
    }
}
