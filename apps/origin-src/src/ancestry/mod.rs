//! Merge bases in a history of immutable records with snapshots: the
//! acceptances of targets, each after the one before it and the
//! acceptances it landed, or the revisions of a change, each after its
//! predecessors.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use origin_platform::Platform;
use origin_shared::TreeId;

use crate::store::{Store, StoreError};

/// A history, as far as it was walked: the parents and the snapshot of
/// each record in it.
#[derive(Debug)]
pub struct Ancestry<I> {
    records: HashMap<I, (Vec<I>, TreeId)>,
}

impl<I: Copy + Eq + Hash + Ord> Ancestry<I> {
    /// Walks everything `tips` descend from, asking `read` for the parents
    /// and the snapshot of each record, once.
    ///
    /// # Errors
    ///
    /// Fails as `read` does.
    pub fn walk<E>(
        tips: &[I],
        mut read: impl FnMut(I) -> Result<(Vec<I>, TreeId), E>,
    ) -> Result<Self, E> {
        let mut records = HashMap::new();
        let mut next = tips.to_vec();
        while let Some(id) = next.pop() {
            if records.contains_key(&id) {
                continue;
            }
            let record = read(id)?;
            next.extend(&record.0);
            records.insert(id, record);
        }
        Ok(Self { records })
    }

    /// Returns the snapshot to merge `tips` from: the snapshot of their best
    /// common ancestor, or `None` if they share no ancestor. When there are
    /// several best common ancestors, as after criss-cross merges, it first
    /// merges them into one, like git's recursive strategy. Where they
    /// conflict, the result holds what [`Store::merge_bases`] leaves, so a
    /// merge from it conflicts there too unless both sides agree. Writes the
    /// trees this creates. Calls `need` with each record before reading its
    /// snapshot, so a lazy clone can fetch it.
    ///
    /// # Errors
    ///
    /// Fails if `need` fails, or if a tree cannot be read or written.
    pub fn merge_base<P: Platform, E: From<StoreError>>(
        &self,
        store: &Store<P>,
        tips: &[I],
        mut need: impl FnMut(I) -> Result<(), E>,
    ) -> Result<Option<TreeId>, E> {
        let groups: Vec<_> = tips.iter().map(std::slice::from_ref).collect();
        self.merge_base_of(store, &groups, &mut need)
    }

    /// The snapshot to merge `groups` from, each standing for all it holds.
    fn merge_base_of<P: Platform, E: From<StoreError>>(
        &self,
        store: &Store<P>,
        groups: &[&[I]],
        need: &mut impl FnMut(I) -> Result<(), E>,
    ) -> Result<Option<TreeId>, E> {
        let bases = self.best_common(groups);
        let Some((&first, others)) = bases.split_first() else {
            return Ok(None);
        };
        let mut merged = vec![first];
        let mut tree = self.snapshot(first, need)?;
        for &other in others {
            let base = self.merge_base_of(store, &[&merged, &[other]], need)?;
            let merge = store.merge_bases(base, tree, self.snapshot(other, need)?)?;
            store.write_all(&merge.trees)?;
            tree = merge.tree;
            merged.push(other);
        }
        Ok(Some(tree))
    }

    /// The best common ancestors of `groups`: the records that every group
    /// has one that is or descends from, but that none of the others
    /// descends from, sorted. Each record is visited a bounded number of
    /// times.
    fn best_common(&self, groups: &[&[I]]) -> Vec<I> {
        let Some((first, others)) = groups.split_first() else {
            return Vec::new();
        };
        let mut common = self.reachable(first.iter().copied());
        for group in others {
            let reachable = self.reachable(group.iter().copied());
            common.retain(|id| reachable.contains(id));
        }
        let parents = common.iter().flat_map(|&id| self.parents(id));
        let below = self.reachable(parents.copied());
        let mut bases: Vec<_> = common.difference(&below).copied().collect();
        bases.sort_unstable();
        bases
    }

    /// The records `from` are or descend from.
    fn reachable(&self, from: impl IntoIterator<Item = I>) -> HashSet<I> {
        let mut reached = HashSet::new();
        let mut next: Vec<_> = from.into_iter().collect();
        while let Some(id) = next.pop() {
            if reached.insert(id) {
                next.extend(self.parents(id));
            }
        }
        reached
    }

    /// Whether `id` is or descends from `ancestor`, as far as the walk
    /// read.
    #[must_use]
    pub fn descends(&self, id: I, ancestor: I) -> bool {
        self.reachable([id]).contains(&ancestor)
    }

    fn parents(&self, id: I) -> &[I] {
        self.records.get(&id).map_or(&[], |(parents, _)| parents)
    }

    /// Calls `need` with `id`, then returns its snapshot.
    fn snapshot<E>(&self, id: I, need: &mut impl FnMut(I) -> Result<(), E>) -> Result<TreeId, E> {
        need(id)?;
        Ok(self.records[&id].1)
    }
}

#[cfg(test)]
mod tests;
