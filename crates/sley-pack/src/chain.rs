//! Heap work stack shared by every targeted delta-chain read.
//!
//! Pack readers, the bounded decoder, and object stores all resolve delta
//! chains through [`resolve_delta_chain`]. Each link of a chain is a heap
//! frame rather than a call frame, so stack use stays constant however deep the
//! chain is, including chains whose REF bases re-enter an object store.

use std::collections::HashSet;
use std::hash::Hash;

/// The base an entry waits on: a storage location still to be entered, or an
/// object that is already decoded.
#[derive(Debug)]
pub enum DeltaChainBase<L, O> {
    /// A storage location the driver enters next.
    Location(L),
    /// A base that is already decoded, for example from a cache or another
    /// object store.
    Resolved(O),
}

/// What [`DeltaChainResolver::enter`] found at a location.
#[derive(Debug)]
pub enum DeltaChainStep<L, E, O> {
    /// The object is already available, for example from a cache.
    Resolved(O),
    /// An entry that needs no base; [`DeltaChainResolver::finish`] receives
    /// `None` for it.
    Base(E),
    /// An entry that needs `base` first. Redirects (an object lookup that is
    /// satisfied by a storage copy) use this with an entry whose `finish`
    /// returns its base unchanged.
    Pending(E, DeltaChainBase<L, O>),
}

/// The [`DeltaChainStep`] produced by resolver `R`.
pub type DeltaChainStepOf<R> = DeltaChainStep<
    <R as DeltaChainResolver>::Location,
    <R as DeltaChainResolver>::Entry,
    <R as DeltaChainResolver>::Object,
>;

/// The [`DeltaChainBase`] produced by resolver `R`.
pub type DeltaChainBaseOf<R> =
    DeltaChainBase<<R as DeltaChainResolver>::Location, <R as DeltaChainResolver>::Object>;

/// Storage and decoding hooks for [`resolve_delta_chain`].
///
/// Hooks never call back into a decoder: an unresolved base is returned as a
/// location for the driver to enter. Limits, cache policy, and error types
/// belong to the resolver.
pub trait DeltaChainResolver {
    /// Identifies one entry in storage. A location that is entered again while
    /// it is still waiting on its base is a cycle.
    type Location: Copy + Eq + Hash;
    /// Per-location state held until the base is resolved.
    type Entry;
    /// A resolved object.
    type Object;
    type Error;

    /// Read the entry at `location`. `pending` is the number of entries
    /// already waiting on this one's result.
    fn enter(
        &mut self,
        location: Self::Location,
        pending: usize,
    ) -> Result<DeltaChainStepOf<Self>, Self::Error>;

    /// Produce the object for `entry` from its resolved base (`None` for a
    /// [`DeltaChainStep::Base`] entry). An error here is reported to the
    /// entry that was waiting on this one.
    fn finish(
        &mut self,
        entry: Self::Entry,
        base: Option<Self::Object>,
    ) -> Result<Self::Object, Self::Error>;

    /// Called when resolving `entry`'s base failed. Returning another base
    /// retries the entry with it (for example a redundant copy elsewhere);
    /// returning an error passes it on to the entry waiting on this one.
    fn recover(
        &mut self,
        _entry: &mut Self::Entry,
        error: Self::Error,
    ) -> Result<DeltaChainBaseOf<Self>, Self::Error> {
        Err(error)
    }

    /// Whether `location` needs cycle detection. A resolver may return
    /// `false` for locations that can only lead to strictly earlier storage,
    /// such as OFS bases, so the walk skips tracking them.
    fn tracks_cycles(&self, _location: &Self::Location) -> bool {
        true
    }

    /// The error reported when a tracked location is entered while it is
    /// still pending.
    fn cycle_error(&self) -> Self::Error;
}

/// Up to this many tracked locations are checked for cycles by a linear scan,
/// which needs no hashing; beyond it a hash set keeps the walk linear in
/// chain length.
const LINEAR_SCAN_DEPTH: usize = 64;

/// The tracked locations currently waiting on a base, innermost last.
struct Active<L> {
    pending: Vec<L>,
    set: Option<HashSet<L>>,
}

impl<L: Copy + Eq + Hash> Active<L> {
    fn contains(&self, location: &L) -> bool {
        match &self.set {
            Some(set) => set.contains(location),
            None => self.pending.contains(location),
        }
    }

    fn insert(&mut self, location: L) {
        self.pending.push(location);
        match &mut self.set {
            Some(set) => {
                set.insert(location);
            }
            None if self.pending.len() > LINEAR_SCAN_DEPTH => {
                self.set = Some(self.pending.iter().copied().collect());
            }
            None => {}
        }
    }

    fn remove(&mut self, location: &L) {
        // Frames unwind innermost first, so this is normally the last entry.
        match self.pending.iter().rposition(|pending| pending == location) {
            Some(index) => {
                self.pending.remove(index);
            }
            None => return,
        }
        if let Some(set) = &mut self.set {
            set.remove(location);
        }
    }
}

/// Resolve the object at `location`, following every base through
/// `resolver` with constant call-stack use. Heap use is proportional to the
/// number of entries waiting on a base.
pub fn resolve_delta_chain<R: DeltaChainResolver>(
    resolver: &mut R,
    location: R::Location,
) -> Result<R::Object, R::Error> {
    let mut frames: Vec<(R::Location, R::Entry)> = Vec::new();
    let mut active = Active {
        pending: Vec::new(),
        set: None,
    };
    let mut next = DeltaChainBase::Location(location);
    'walk: loop {
        let mut result = match next {
            DeltaChainBase::Resolved(object) => Ok(object),
            DeltaChainBase::Location(location)
                if resolver.tracks_cycles(&location) && active.contains(&location) =>
            {
                Err(resolver.cycle_error())
            }
            DeltaChainBase::Location(location) => match resolver.enter(location, frames.len()) {
                Ok(DeltaChainStep::Pending(entry, base)) => {
                    if resolver.tracks_cycles(&location) {
                        active.insert(location);
                    }
                    frames.push((location, entry));
                    next = base;
                    continue 'walk;
                }
                Ok(DeltaChainStep::Resolved(object)) => Ok(object),
                Ok(DeltaChainStep::Base(entry)) => resolver.finish(entry, None),
                Err(error) => Err(error),
            },
        };
        while let Some((location, mut entry)) = frames.pop() {
            result = match result {
                Ok(object) => resolver.finish(entry, Some(object)),
                Err(error) => match resolver.recover(&mut entry, error) {
                    // Retry with another base; the location stays pending.
                    Ok(base) => {
                        frames.push((location, entry));
                        next = base;
                        continue 'walk;
                    }
                    Err(error) => Err(error),
                },
            };
            if resolver.tracks_cycles(&location) {
                active.remove(&location);
            }
        }
        return result;
    }
}
