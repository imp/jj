// Copyright 2023 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A lazily merged view of a set of trees.

use std::borrow::Borrow;
use std::cmp::max;
use std::collections::BTreeMap;
use std::iter;
use std::iter::zip;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
use std::vec;

use either::Either;
use futures::Stream;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::future::try_join;
use futures::stream::BoxStream;
use itertools::EitherOrBoth;
use itertools::Itertools as _;
use pollster::FutureExt as _;

use crate::backend;
use crate::backend::BackendResult;
use crate::backend::MergedTreeId;
use crate::backend::TreeId;
use crate::backend::TreeValue;
use crate::copies::CopiesTreeDiffEntry;
use crate::copies::CopiesTreeDiffStream;
use crate::copies::CopyRecords;
use crate::matchers::EverythingMatcher;
use crate::matchers::Matcher;
use crate::merge::Merge;
use crate::merge::MergeBuilder;
use crate::merge::MergedTreeVal;
use crate::merge::MergedTreeValue;
use crate::repo_path::RepoPath;
use crate::repo_path::RepoPathBuf;
use crate::repo_path::RepoPathComponent;
use crate::store::Store;
use crate::tree::Tree;
use crate::tree::try_resolve_file_conflict;
use crate::tree_builder::TreeBuilder;

/// Presents a view of a merged set of trees.
#[derive(PartialEq, Eq, Clone, Debug)]
pub struct MergedTree {
    trees: Merge<Tree>,
}

impl MergedTree {
    /// Creates a new `MergedTree` representing a single tree without conflicts.
    pub fn resolved(tree: Tree) -> Self {
        Self::new(Merge::resolved(tree))
    }

    /// Creates a new `MergedTree` representing a merge of a set of trees. The
    /// individual trees must not have any conflicts.
    pub fn new(trees: Merge<Tree>) -> Self {
        debug_assert!(!trees.iter().any(|t| t.has_conflict()));
        debug_assert!(trees.iter().map(|tree| tree.dir()).all_equal());
        debug_assert!(
            trees
                .iter()
                .map(|tree| Arc::as_ptr(tree.store()))
                .all_equal()
        );
        Self { trees }
    }

    /// Takes a tree in the legacy format (with path-level conflicts in the
    /// tree) and returns a `MergedTree` with any conflicts converted to
    /// tree-level conflicts.
    pub fn from_legacy_tree(tree: Tree) -> BackendResult<Self> {
        let conflict_ids = tree.conflicts();
        if conflict_ids.is_empty() {
            return Ok(Self::resolved(tree));
        }

        // Find the number of removes and adds in the most complex conflict.
        let mut max_tree_count = 1;
        let store = tree.store();
        let mut conflicts: Vec<(&RepoPath, MergedTreeValue)> = vec![];
        for (path, conflict_id) in &conflict_ids {
            let conflict = store.read_conflict(path, conflict_id)?;
            max_tree_count = max(max_tree_count, conflict.iter().len());
            conflicts.push((path, conflict));
        }
        let mut tree_builders = Vec::new();
        tree_builders.resize_with(max_tree_count, || {
            TreeBuilder::new(store.clone(), tree.id().clone())
        });
        for (path, conflict) in conflicts {
            // If there are fewer terms in this conflict than in some other conflict, we can
            // add canceling removes and adds of any value. The simplest value is an absent
            // value, so we use that.
            let terms_padded = conflict.into_iter().chain(iter::repeat(None));
            for (builder, term) in zip(&mut tree_builders, terms_padded) {
                builder.set_or_remove(path.to_owned(), term);
            }
        }

        let new_trees: Vec<_> = tree_builders
            .into_iter()
            .map(|builder| {
                let tree_id = builder.write_tree()?;
                store.get_tree(RepoPathBuf::root(), &tree_id)
            })
            .try_collect()?;
        Ok(Self {
            trees: Merge::from_vec(new_trees),
        })
    }

    /// Returns the underlying `Merge<Tree>`.
    pub fn as_merge(&self) -> &Merge<Tree> {
        &self.trees
    }

    /// Extracts the underlying `Merge<Tree>`.
    pub fn take(self) -> Merge<Tree> {
        self.trees
    }

    /// This tree's directory
    pub fn dir(&self) -> &RepoPath {
        self.trees.first().dir()
    }

    /// The `Store` associated with this tree.
    pub fn store(&self) -> &Arc<Store> {
        self.trees.first().store()
    }

    /// Base names of entries in this directory.
    pub fn names<'a>(&'a self) -> Box<dyn Iterator<Item = &'a RepoPathComponent> + 'a> {
        Box::new(all_tree_basenames(&self.trees))
    }

    /// The value at the given basename. The value can be `Resolved` even if
    /// `self` is a `Merge`, which happens if the value at the path can be
    /// trivially merged. Does not recurse, so if `basename` refers to a Tree,
    /// then a `TreeValue::Tree` will be returned.
    pub fn value(&self, basename: &RepoPathComponent) -> MergedTreeVal<'_> {
        trees_value(&self.trees, basename)
    }

    /// Tries to resolve any conflicts, resolving any conflicts that can be
    /// automatically resolved and leaving the rest unresolved.
    pub async fn resolve(self) -> BackendResult<Self> {
        let merged = merge_trees(self.trees).await?;
        // If the result can be resolved, then `merge_trees()` above would have returned
        // a resolved merge. However, that function will always preserve the arity of
        // conflicts it cannot resolve. So we simplify the conflict again
        // here to possibly reduce a complex conflict to a simpler one.
        let simplified = merged.simplify();
        // If debug assertions are enabled, check that the merge was idempotent. In
        // particular,  that this last simplification doesn't enable further automatic
        // resolutions
        if cfg!(debug_assertions) {
            let re_merged = merge_trees(simplified.clone()).await.unwrap();
            debug_assert_eq!(re_merged, simplified);
        }
        Ok(Self { trees: simplified })
    }

    /// An iterator over the conflicts in this tree, including subtrees.
    /// Recurses into subtrees and yields conflicts in those, but only if
    /// all sides are trees, so tree/file conflicts will be reported as a single
    /// conflict, not one for each path in the tree.
    // TODO: Restrict this by a matcher (or add a separate method for that).
    pub fn conflicts(
        &self,
    ) -> impl Iterator<Item = (RepoPathBuf, BackendResult<MergedTreeValue>)> + use<> {
        ConflictIterator::new(self)
    }

    /// Whether this tree has conflicts.
    pub fn has_conflict(&self) -> bool {
        !self.trees.is_resolved()
    }

    /// Gets the `MergeTree` in a subdirectory of the current tree. If the path
    /// doesn't correspond to a tree in any of the inputs to the merge, then
    /// that entry will be replace by an empty tree in the result.
    pub async fn sub_tree(&self, name: &RepoPathComponent) -> BackendResult<Option<Self>> {
        match self.value(name).into_resolved() {
            Ok(Some(TreeValue::Tree(sub_tree_id))) => {
                let subdir = self.dir().join(name);
                Ok(Some(Self::resolved(
                    self.store().get_tree_async(subdir, sub_tree_id).await?,
                )))
            }
            Ok(_) => Ok(None),
            Err(merge) => {
                if !merge.is_tree() {
                    return Ok(None);
                }
                let trees = merge
                    .try_map_async(async |value| match value {
                        Some(TreeValue::Tree(sub_tree_id)) => {
                            let subdir = self.dir().join(name);
                            self.store().get_tree_async(subdir, sub_tree_id).await
                        }
                        Some(_) => unreachable!(),
                        None => {
                            let subdir = self.dir().join(name);
                            Ok(Tree::empty(self.store().clone(), subdir))
                        }
                    })
                    .await?;
                Ok(Some(Self { trees }))
            }
        }
    }

    /// The value at the given path. The value can be `Resolved` even if
    /// `self` is a `Conflict`, which happens if the value at the path can be
    /// trivially merged.
    pub fn path_value(&self, path: &RepoPath) -> BackendResult<MergedTreeValue> {
        self.path_value_async(path).block_on()
    }

    /// Async version of `path_value()`.
    pub async fn path_value_async(&self, path: &RepoPath) -> BackendResult<MergedTreeValue> {
        assert_eq!(self.dir(), RepoPath::root());
        match path.split() {
            Some((dir, basename)) => match self.sub_tree_recursive(dir).await? {
                None => Ok(Merge::absent()),
                Some(tree) => Ok(tree.value(basename).cloned()),
            },
            None => Ok(self
                .trees
                .map(|tree| Some(TreeValue::Tree(tree.id().clone())))),
        }
    }

    /// The tree's id
    pub fn id(&self) -> MergedTreeId {
        MergedTreeId::Merge(self.trees.map(|tree| tree.id().clone()))
    }

    /// Look up the tree at the given path.
    pub async fn sub_tree_recursive(&self, path: &RepoPath) -> BackendResult<Option<Self>> {
        let mut current_tree = self.clone();
        for name in path.components() {
            match current_tree.sub_tree(name).await? {
                None => {
                    return Ok(None);
                }
                Some(sub_tree) => {
                    current_tree = sub_tree;
                }
            }
        }
        Ok(Some(current_tree))
    }

    /// Iterator over the entries matching the given matcher. Subtrees are
    /// visited recursively. Subtrees that differ between the current
    /// `MergedTree`'s terms are merged on the fly. Missing terms are treated as
    /// empty directories. Subtrees that conflict with non-trees are not
    /// visited. For example, if current tree is a merge of 3 trees, and the
    /// entry for 'foo' is a conflict between a change subtree and a symlink
    /// (i.e. the subdirectory was replaced by symlink in one side of the
    /// conflict), then the entry for `foo` itself will be emitted, but no
    /// entries from inside `foo/` from either of the trees will be.
    pub fn entries(&self) -> TreeEntriesIterator<'static> {
        self.entries_matching(&EverythingMatcher)
    }

    /// Like `entries()` but restricted by a matcher.
    pub fn entries_matching<'matcher>(
        &self,
        matcher: &'matcher dyn Matcher,
    ) -> TreeEntriesIterator<'matcher> {
        TreeEntriesIterator::new(&self.trees, matcher)
    }

    /// Stream of the differences between this tree and another tree.
    ///
    /// Tree entries (`MergedTreeValue::is_tree()`) are included only if the
    /// other side is present and not a tree.
    fn diff_stream_internal<'matcher>(
        &self,
        other: &Self,
        matcher: &'matcher dyn Matcher,
    ) -> TreeDiffStream<'matcher> {
        let concurrency = self.store().concurrency();
        if concurrency <= 1 {
            Box::pin(futures::stream::iter(TreeDiffIterator::new(
                &self.trees,
                &other.trees,
                matcher,
            )))
        } else {
            Box::pin(TreeDiffStreamImpl::new(
                &self.trees,
                &other.trees,
                matcher,
                concurrency,
            ))
        }
    }

    /// Stream of the differences between this tree and another tree.
    pub fn diff_stream<'matcher>(
        &self,
        other: &Self,
        matcher: &'matcher dyn Matcher,
    ) -> TreeDiffStream<'matcher> {
        stream_without_trees(self.diff_stream_internal(other, matcher))
    }

    /// Like `diff_stream()` but files in a removed tree will be returned before
    /// a file that replaces it.
    pub fn diff_stream_for_file_system<'matcher>(
        &self,
        other: &Self,
        matcher: &'matcher dyn Matcher,
    ) -> TreeDiffStream<'matcher> {
        Box::pin(DiffStreamForFileSystem::new(
            self.diff_stream_internal(other, matcher),
        ))
    }

    /// Like `diff_stream()` but takes the given copy records into account.
    pub fn diff_stream_with_copies<'a>(
        &self,
        other: &Self,
        matcher: &'a dyn Matcher,
        copy_records: &'a CopyRecords,
    ) -> BoxStream<'a, CopiesTreeDiffEntry> {
        let stream = self.diff_stream(other, matcher);
        Box::pin(CopiesTreeDiffStream::new(
            stream,
            self.clone(),
            other.clone(),
            copy_records,
        ))
    }

    /// Merges this tree with `other`, using `base` as base. Any conflicts will
    /// be resolved recursively if possible.
    pub async fn merge(self, base: Self, other: Self) -> BackendResult<Self> {
        self.merge_no_resolve(base, other).resolve().await
    }

    /// Merges this tree with `other`, using `base` as base, without attempting
    /// to resolve file conflicts.
    pub fn merge_no_resolve(self, base: Self, other: Self) -> Self {
        let nested = Merge::from_vec(vec![self.trees, base.trees, other.trees]);
        Self {
            trees: nested.flatten().simplify(),
        }
    }
}

/// A single entry in a tree diff.
pub struct TreeDiffEntry {
    /// The path.
    pub path: RepoPathBuf,
    /// The resolved tree values if available.
    pub values: BackendResult<(MergedTreeValue, MergedTreeValue)>,
}

/// Type alias for the result from `MergedTree::diff_stream()`. We use a
/// `Stream` instead of an `Iterator` so high-latency backends (e.g. cloud-based
/// ones) can fetch trees asynchronously.
pub type TreeDiffStream<'matcher> = BoxStream<'matcher, TreeDiffEntry>;

fn all_tree_basenames(trees: &Merge<Tree>) -> impl Iterator<Item = &RepoPathComponent> {
    trees
        .iter()
        .map(|tree| tree.data().names())
        .kmerge()
        .dedup()
}

fn all_tree_entries(
    trees: &Merge<Tree>,
) -> impl Iterator<Item = (&RepoPathComponent, MergedTreeVal<'_>)> {
    if let Some(tree) = trees.as_resolved() {
        let iter = tree
            .entries_non_recursive()
            .map(|entry| (entry.name(), Merge::normal(entry.value())));
        Either::Left(iter)
    } else {
        let iter = all_merged_tree_entries(trees).map(|(name, values)| {
            // TODO: move resolve_trivial() to caller?
            let values = match values.resolve_trivial() {
                Some(resolved) => Merge::resolved(*resolved),
                None => values,
            };
            (name, values)
        });
        Either::Right(iter)
    }
}

/// Suppose the given `trees` aren't resolved, iterates `(name, values)` pairs
/// non-recursively. This also works if `trees` are resolved, but is more costly
/// than `tree.entries_non_recursive()`.
fn all_merged_tree_entries(
    trees: &Merge<Tree>,
) -> impl Iterator<Item = (&RepoPathComponent, MergedTreeVal<'_>)> {
    let mut entries_iters = trees
        .iter()
        .map(|tree| tree.entries_non_recursive().peekable())
        .collect_vec();
    iter::from_fn(move || {
        let next_name = entries_iters
            .iter_mut()
            .filter_map(|iter| iter.peek())
            .map(|entry| entry.name())
            .min()?;
        let values: MergeBuilder<_> = entries_iters
            .iter_mut()
            .map(|iter| {
                let entry = iter.next_if(|entry| entry.name() == next_name)?;
                Some(entry.value())
            })
            .collect();
        Some((next_name, values.build()))
    })
}

fn merged_tree_entry_diff<'a>(
    trees1: &'a Merge<Tree>,
    trees2: &'a Merge<Tree>,
) -> impl Iterator<Item = (&'a RepoPathComponent, MergedTreeVal<'a>, MergedTreeVal<'a>)> {
    itertools::merge_join_by(
        all_tree_entries(trees1),
        all_tree_entries(trees2),
        |(name1, _), (name2, _)| name1.cmp(name2),
    )
    .map(|entry| match entry {
        EitherOrBoth::Both((name, value1), (_, value2)) => (name, value1, value2),
        EitherOrBoth::Left((name, value1)) => (name, value1, Merge::absent()),
        EitherOrBoth::Right((name, value2)) => (name, Merge::absent(), value2),
    })
    .filter(|(_, value1, value2)| value1 != value2)
}

fn trees_value<'a>(trees: &'a Merge<Tree>, basename: &RepoPathComponent) -> MergedTreeVal<'a> {
    if let Some(tree) = trees.as_resolved() {
        return Merge::resolved(tree.value(basename));
    }
    let value = trees.map(|tree| tree.value(basename));
    if let Some(resolved) = value.resolve_trivial() {
        return Merge::resolved(*resolved);
    }
    value
}

/// The returned conflict will either be resolved or have the same number of
/// sides as the input.
async fn merge_trees(merge: Merge<Tree>) -> BackendResult<Merge<Tree>> {
    let merge = match merge.into_resolved() {
        Ok(tree) => return Ok(Merge::resolved(tree)),
        Err(merge) => merge,
    };

    let base_tree = merge.first();
    let store = base_tree.store();
    let dir = base_tree.dir();
    // Keep resolved entries in `new_tree` and conflicted entries in `conflicts` to
    // start with. Then we'll create the full trees later, and only if there are
    // any conflicts.
    let mut new_tree_entries = Vec::new();
    let mut conflicts = vec![];
    // TODO: Merge values concurrently
    for (basename, path_merge) in all_merged_tree_entries(&merge) {
        let path = dir.join(basename);
        let path_merge = merge_tree_values(store, &path, &path_merge).await?;
        match path_merge.into_resolved() {
            Ok(Some(value)) => {
                new_tree_entries.push((basename.to_owned(), value));
            }
            Ok(None) => {}
            Err(path_merge) => {
                conflicts.push((basename, path_merge.into_iter()));
            }
        };
    }
    if conflicts.is_empty() {
        let data = backend::Tree::from_sorted_entries(new_tree_entries);
        let new_tree_id = store.write_tree(dir, data).await?;
        Ok(Merge::resolved(new_tree_id))
    } else {
        // For each side of the conflict, overwrite the entries in `new_tree` with the
        // values from  `conflicts`. Entries that are not in `conflicts` will remain
        // unchanged and will be reused for each side.
        let tree_count = merge.iter().len();
        let mut new_trees = Vec::with_capacity(tree_count);
        // TODO: can merge-join common tree entries with conflicting ones
        let new_tree_entries = BTreeMap::from_iter(new_tree_entries);
        for _ in 0..tree_count {
            let mut new_tree_entries = new_tree_entries.clone();
            for (basename, path_conflict) in &mut conflicts {
                if let Some(value) = path_conflict.next().unwrap() {
                    new_tree_entries.insert(basename.to_owned(), value);
                }
            }
            let data = backend::Tree::from_sorted_entries(new_tree_entries.into_iter().collect());
            let tree = store.write_tree(dir, data).await?;
            new_trees.push(tree);
        }
        Ok(Merge::from_vec(new_trees))
    }
}

/// Tries to resolve a conflict between tree values. Returns
/// Ok(Merge::normal(value)) if the conflict was resolved, and
/// Ok(Merge::absent()) if the path should be removed. Returns the
/// conflict unmodified if it cannot be resolved automatically.
async fn merge_tree_values(
    store: &Arc<Store>,
    path: &RepoPath,
    values: &MergedTreeVal<'_>,
) -> BackendResult<MergedTreeValue> {
    if let Some(resolved) = values.resolve_trivial() {
        return Ok(Merge::resolved(resolved.cloned()));
    }

    if let Some(trees) = values.to_tree_merge(store, path).await? {
        // If all sides are trees or missing, merge the trees recursively, treating
        // missing trees as empty.
        let empty_tree_id = store.empty_tree_id();
        let merged_tree = Box::pin(merge_trees(trees)).await?;
        Ok(merged_tree
            .map(|tree| (tree.id() != empty_tree_id).then(|| TreeValue::Tree(tree.id().clone()))))
    } else {
        let maybe_resolved = try_resolve_file_values(store, path, values).await?;
        Ok(maybe_resolved.unwrap_or_else(|| values.cloned()))
    }
}

/// Tries to resolve file conflicts by merging the file contents. Treats missing
/// files as empty. If the file conflict cannot be resolved, returns the passed
/// `values` unmodified.
pub async fn resolve_file_values(
    store: &Arc<Store>,
    path: &RepoPath,
    values: MergedTreeValue,
) -> BackendResult<MergedTreeValue> {
    if let Some(resolved) = values.resolve_trivial() {
        return Ok(Merge::resolved(resolved.clone()));
    }

    let maybe_resolved = try_resolve_file_values(store, path, &values).await?;
    Ok(maybe_resolved.unwrap_or(values))
}

async fn try_resolve_file_values<T: Borrow<TreeValue>>(
    store: &Arc<Store>,
    path: &RepoPath,
    values: &Merge<Option<T>>,
) -> BackendResult<Option<MergedTreeValue>> {
    // The values may contain trees canceling each other (notably padded absent
    // trees), so we need to simplify them first.
    let simplified = values
        .map(|value| value.as_ref().map(Borrow::borrow))
        .simplify();
    // No fast path for simplified.is_resolved(). If it could be resolved, it would
    // have been caught by values.resolve_trivial() above.
    if let Some(resolved) = try_resolve_file_conflict(store, path, &simplified).await? {
        Ok(Some(Merge::normal(resolved)))
    } else {
        // Failed to merge the files, or the paths are not files
        Ok(None)
    }
}

/// Recursive iterator over the entries in a tree.
pub struct TreeEntriesIterator<'matcher> {
    store: Arc<Store>,
    stack: Vec<TreeEntriesDirItem>,
    matcher: &'matcher dyn Matcher,
}

struct TreeEntriesDirItem {
    entries: Vec<(RepoPathBuf, MergedTreeValue)>,
}

impl TreeEntriesDirItem {
    fn new(trees: &Merge<Tree>, matcher: &dyn Matcher) -> Self {
        let mut entries = vec![];
        let dir = trees.first().dir();
        for (name, value) in all_tree_entries(trees) {
            let path = dir.join(name);
            if value.is_tree() {
                // TODO: Handle the other cases (specific files and trees)
                if matcher.visit(&path).is_nothing() {
                    continue;
                }
            } else if !matcher.matches(&path) {
                continue;
            }
            entries.push((path, value.cloned()));
        }
        entries.reverse();
        Self { entries }
    }
}

impl<'matcher> TreeEntriesIterator<'matcher> {
    fn new(trees: &Merge<Tree>, matcher: &'matcher dyn Matcher) -> Self {
        Self {
            store: trees.first().store().clone(),
            stack: vec![TreeEntriesDirItem::new(trees, matcher)],
            matcher,
        }
    }
}

impl Iterator for TreeEntriesIterator<'_> {
    type Item = (RepoPathBuf, BackendResult<MergedTreeValue>);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(top) = self.stack.last_mut() {
            if let Some((path, value)) = top.entries.pop() {
                let maybe_trees = match value.to_tree_merge(&self.store, &path).block_on() {
                    Ok(maybe_trees) => maybe_trees,
                    Err(err) => return Some((path, Err(err))),
                };
                if let Some(trees) = maybe_trees {
                    self.stack
                        .push(TreeEntriesDirItem::new(&trees, self.matcher));
                } else {
                    return Some((path, Ok(value)));
                }
            } else {
                self.stack.pop();
            }
        }
        None
    }
}

/// The state for the non-recursive iteration over the conflicted entries in a
/// single directory.
struct ConflictsDirItem {
    entries: Vec<(RepoPathBuf, MergedTreeValue)>,
}

impl From<&Merge<Tree>> for ConflictsDirItem {
    fn from(trees: &Merge<Tree>) -> Self {
        let dir = trees.first().dir();
        if trees.is_resolved() {
            return Self { entries: vec![] };
        }

        let mut entries = vec![];
        for (basename, value) in all_tree_entries(trees) {
            if !value.is_resolved() {
                entries.push((dir.join(basename), value.cloned()));
            }
        }
        entries.reverse();
        Self { entries }
    }
}

struct ConflictIterator {
    store: Arc<Store>,
    stack: Vec<ConflictsDirItem>,
}

impl ConflictIterator {
    fn new(tree: &MergedTree) -> Self {
        Self {
            store: tree.store().clone(),
            stack: vec![ConflictsDirItem::from(&tree.trees)],
        }
    }
}

impl Iterator for ConflictIterator {
    type Item = (RepoPathBuf, BackendResult<MergedTreeValue>);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(top) = self.stack.last_mut() {
            if let Some((path, tree_values)) = top.entries.pop() {
                match tree_values.to_tree_merge(&self.store, &path).block_on() {
                    Ok(Some(trees)) => {
                        // If all sides are trees or missing, descend into the merged tree
                        self.stack.push(ConflictsDirItem::from(&trees));
                    }
                    Ok(None) => {
                        // Otherwise this is a conflict between files, trees, etc. If they could
                        // be automatically resolved, they should have been when the top-level
                        // tree conflict was written, so we assume that they can't be.
                        return Some((path, Ok(tree_values)));
                    }
                    Err(err) => {
                        return Some((path, Err(err)));
                    }
                }
            } else {
                self.stack.pop();
            }
        }
        None
    }
}

/// Iterator over the differences between two trees.
///
/// Tree entries (`MergedTreeValue::is_tree()`) are included only if the other
/// side is present and not a tree.
pub struct TreeDiffIterator<'matcher> {
    store: Arc<Store>,
    stack: Vec<TreeDiffDir>,
    matcher: &'matcher dyn Matcher,
}

struct TreeDiffDir {
    entries: Vec<(RepoPathBuf, MergedTreeValue, MergedTreeValue)>,
}

impl<'matcher> TreeDiffIterator<'matcher> {
    /// Creates a iterator over the differences between two trees.
    pub fn new(trees1: &Merge<Tree>, trees2: &Merge<Tree>, matcher: &'matcher dyn Matcher) -> Self {
        assert!(Arc::ptr_eq(trees1.first().store(), trees2.first().store()));
        let root_dir = RepoPath::root();
        let mut stack = Vec::new();
        if !matcher.visit(root_dir).is_nothing() {
            stack.push(TreeDiffDir::from_trees(root_dir, trees1, trees2, matcher));
        };
        Self {
            store: trees1.first().store().clone(),
            stack,
            matcher,
        }
    }

    /// Gets the given trees if `values` are trees, otherwise an empty tree.
    fn trees(
        store: &Arc<Store>,
        dir: &RepoPath,
        values: &MergedTreeValue,
    ) -> BackendResult<Merge<Tree>> {
        if let Some(trees) = values.to_tree_merge(store, dir).block_on()? {
            Ok(trees)
        } else {
            Ok(Merge::resolved(Tree::empty(store.clone(), dir.to_owned())))
        }
    }
}

impl TreeDiffDir {
    fn from_trees(
        dir: &RepoPath,
        trees1: &Merge<Tree>,
        trees2: &Merge<Tree>,
        matcher: &dyn Matcher,
    ) -> Self {
        let mut entries = vec![];
        for (name, before, after) in merged_tree_entry_diff(trees1, trees2) {
            let path = dir.join(name);
            let tree_before = before.is_tree();
            let tree_after = after.is_tree();
            // Check if trees and files match, but only if either side is a tree or a file
            // (don't query the matcher unnecessarily).
            let tree_matches = (tree_before || tree_after) && !matcher.visit(&path).is_nothing();
            let file_matches = (!tree_before || !tree_after) && matcher.matches(&path);

            // Replace trees or files that don't match by `Merge::absent()`
            let before = if (tree_before && tree_matches) || (!tree_before && file_matches) {
                before
            } else {
                Merge::absent()
            };
            let after = if (tree_after && tree_matches) || (!tree_after && file_matches) {
                after
            } else {
                Merge::absent()
            };
            if before.is_absent() && after.is_absent() {
                continue;
            }
            entries.push((path, before.cloned(), after.cloned()));
        }
        entries.reverse();
        Self { entries }
    }
}

impl Iterator for TreeDiffIterator<'_> {
    type Item = TreeDiffEntry;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(top) = self.stack.last_mut() {
            let (path, before, after) = match top.entries.pop() {
                Some(entry) => entry,
                None => {
                    self.stack.pop().unwrap();
                    continue;
                }
            };

            if before.is_tree() || after.is_tree() {
                let (before_tree, after_tree) = match (
                    Self::trees(&self.store, &path, &before),
                    Self::trees(&self.store, &path, &after),
                ) {
                    (Ok(before_tree), Ok(after_tree)) => (before_tree, after_tree),
                    (Err(before_err), _) => {
                        return Some(TreeDiffEntry {
                            path,
                            values: Err(before_err),
                        });
                    }
                    (_, Err(after_err)) => {
                        return Some(TreeDiffEntry {
                            path,
                            values: Err(after_err),
                        });
                    }
                };
                let subdir =
                    TreeDiffDir::from_trees(&path, &before_tree, &after_tree, self.matcher);
                self.stack.push(subdir);
            };
            if before.is_file_like() || after.is_file_like() {
                return Some(TreeDiffEntry {
                    path,
                    values: Ok((before, after)),
                });
            }
        }
        None
    }
}

/// Stream of differences between two trees.
///
/// Tree entries (`MergedTreeValue::is_tree()`) are included only if the other
/// side is present and not a tree.
pub struct TreeDiffStreamImpl<'matcher> {
    store: Arc<Store>,
    matcher: &'matcher dyn Matcher,
    /// Pairs of tree values that may or may not be ready to emit, sorted in the
    /// order we want to emit them. If either side is a tree, there will be
    /// a corresponding entry in `pending_trees`. The item is ready to emit
    /// unless there's a smaller or equal path in `pending_trees`.
    items: BTreeMap<RepoPathBuf, BackendResult<(MergedTreeValue, MergedTreeValue)>>,
    // TODO: Is it better to combine this and `items` into a single map?
    #[expect(clippy::type_complexity)]
    pending_trees:
        BTreeMap<RepoPathBuf, BoxFuture<'matcher, BackendResult<(Merge<Tree>, Merge<Tree>)>>>,
    /// The maximum number of trees to request concurrently. However, we do the
    /// accounting per path, so there will often be twice as many pending
    /// `Backend::read_tree()` calls - for the "before" and "after" sides. For
    /// conflicts, there will be even more.
    max_concurrent_reads: usize,
    /// The maximum number of items in `items`. However, we will always add the
    /// full differences from a particular pair of trees, so it may temporarily
    /// go over the limit (until we emit those items). It may also go over the
    /// limit because we have a file item that's blocked by pending subdirectory
    /// items.
    max_queued_items: usize,
}

impl<'matcher> TreeDiffStreamImpl<'matcher> {
    /// Creates a iterator over the differences between two trees. Generally
    /// prefer `MergedTree::diff_stream()` of calling this directly.
    pub fn new(
        trees1: &Merge<Tree>,
        trees2: &Merge<Tree>,
        matcher: &'matcher dyn Matcher,
        max_concurrent_reads: usize,
    ) -> Self {
        assert!(Arc::ptr_eq(trees1.first().store(), trees2.first().store()));
        let mut stream = Self {
            store: trees1.first().store().clone(),
            matcher,
            items: BTreeMap::new(),
            pending_trees: BTreeMap::new(),
            max_concurrent_reads,
            max_queued_items: 10000,
        };
        stream.add_dir_diff_items(RepoPath::root(), trees1, trees2);
        stream
    }

    async fn single_tree(
        store: &Arc<Store>,
        dir: RepoPathBuf,
        value: Option<&TreeValue>,
    ) -> BackendResult<Tree> {
        match value {
            Some(TreeValue::Tree(tree_id)) => store.get_tree_async(dir, tree_id).await,
            _ => Ok(Tree::empty(store.clone(), dir.clone())),
        }
    }

    /// Gets the given trees if `values` are trees, otherwise an empty tree.
    async fn trees(
        store: Arc<Store>,
        dir: RepoPathBuf,
        values: MergedTreeValue,
    ) -> BackendResult<Merge<Tree>> {
        if values.is_tree() {
            values
                .try_map_async(|value| Self::single_tree(&store, dir.clone(), value.as_ref()))
                .await
        } else {
            Ok(Merge::resolved(Tree::empty(store, dir)))
        }
    }

    fn add_dir_diff_items(&mut self, dir: &RepoPath, trees1: &Merge<Tree>, trees2: &Merge<Tree>) {
        for (basename, before, after) in merged_tree_entry_diff(trees1, trees2) {
            let path = dir.join(basename);
            let tree_before = before.is_tree();
            let tree_after = after.is_tree();
            // Check if trees and files match, but only if either side is a tree or a file
            // (don't query the matcher unnecessarily).
            let tree_matches =
                (tree_before || tree_after) && !self.matcher.visit(&path).is_nothing();
            let file_matches = (!tree_before || !tree_after) && self.matcher.matches(&path);

            // Replace trees or files that don't match by `Merge::absent()`
            let before = if (tree_before && tree_matches) || (!tree_before && file_matches) {
                before
            } else {
                Merge::absent()
            };
            let after = if (tree_after && tree_matches) || (!tree_after && file_matches) {
                after
            } else {
                Merge::absent()
            };
            if before.is_absent() && after.is_absent() {
                continue;
            }

            // If the path was a tree on either side of the diff, read those trees.
            if tree_matches {
                let before_tree_future =
                    Self::trees(self.store.clone(), path.clone(), before.cloned());
                let after_tree_future =
                    Self::trees(self.store.clone(), path.clone(), after.cloned());
                let both_trees_future = try_join(before_tree_future, after_tree_future);
                self.pending_trees
                    .insert(path.clone(), Box::pin(both_trees_future));
            }

            if before.is_file_like() || after.is_file_like() {
                self.items
                    .insert(path, Ok((before.cloned(), after.cloned())));
            }
        }
    }

    fn poll_tree_futures(&mut self, cx: &mut Context<'_>) {
        loop {
            let mut tree_diffs = vec![];
            let mut some_pending = false;
            let mut all_pending = true;
            for (dir, future) in self
                .pending_trees
                .iter_mut()
                .take(self.max_concurrent_reads)
            {
                if let Poll::Ready(tree_diff) = future.as_mut().poll(cx) {
                    all_pending = false;
                    tree_diffs.push((dir.clone(), tree_diff));
                } else {
                    some_pending = true;
                }
            }

            for (dir, tree_diff) in tree_diffs {
                let _ = self.pending_trees.remove_entry(&dir).unwrap();
                match tree_diff {
                    Ok((trees1, trees2)) => {
                        self.add_dir_diff_items(&dir, &trees1, &trees2);
                    }
                    Err(err) => {
                        self.items.insert(dir, Err(err));
                    }
                }
            }

            // If none of the futures have been polled and returned `Poll::Pending`, we must
            // not return. If we did, nothing would call the waker so we might never get
            // polled again.
            if all_pending || (some_pending && self.items.len() >= self.max_queued_items) {
                return;
            }
        }
    }
}

impl Stream for TreeDiffStreamImpl<'_> {
    type Item = TreeDiffEntry;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Go through all pending tree futures and poll them.
        self.poll_tree_futures(cx);

        // Now emit the first file, or the first tree that completed with an error
        if let Some((path, _)) = self.items.first_key_value() {
            // Check if there are any pending trees before this item that we need to finish
            // polling before we can emit this item.
            if let Some((dir, _)) = self.pending_trees.first_key_value() {
                if dir < path {
                    return Poll::Pending;
                }
            }

            let (path, values) = self.items.pop_first().unwrap();
            Poll::Ready(Some(TreeDiffEntry { path, values }))
        } else if self.pending_trees.is_empty() {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

fn stream_without_trees(stream: TreeDiffStream) -> TreeDiffStream {
    Box::pin(stream.map(|mut entry| {
        let skip_tree = |merge: MergedTreeValue| {
            if merge.is_tree() {
                Merge::absent()
            } else {
                merge
            }
        };
        entry.values = entry
            .values
            .map(|(before, after)| (skip_tree(before), skip_tree(after)));
        entry
    }))
}

/// Adapts a `TreeDiffStream` to emit a added file at a given path after a
/// removed directory at the same path.
struct DiffStreamForFileSystem<'a> {
    inner: TreeDiffStream<'a>,
    next_item: Option<TreeDiffEntry>,
    held_file: Option<TreeDiffEntry>,
}

impl<'a> DiffStreamForFileSystem<'a> {
    fn new(inner: TreeDiffStream<'a>) -> Self {
        Self {
            inner,
            next_item: None,
            held_file: None,
        }
    }
}

impl Stream for DiffStreamForFileSystem<'_> {
    type Item = TreeDiffEntry;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        while let Some(next) = match self.next_item.take() {
            Some(next) => Some(next),
            None => ready!(self.inner.as_mut().poll_next(cx)),
        } {
            // If there's a held file "foo" and the next item to emit is not "foo/...", then
            // we must be done with the "foo/" directory and it's time to emit "foo" as a
            // removed file.
            if let Some(held_entry) = self
                .held_file
                .take_if(|held_entry| !next.path.starts_with(&held_entry.path))
            {
                self.next_item = Some(next);
                return Poll::Ready(Some(held_entry));
            }

            match next.values {
                Ok((before, after)) if before.is_tree() => {
                    assert!(after.is_present());
                    assert!(self.held_file.is_none());
                    self.held_file = Some(TreeDiffEntry {
                        path: next.path,
                        values: Ok((Merge::absent(), after)),
                    });
                }
                Ok((before, after)) if after.is_tree() => {
                    assert!(before.is_present());
                    return Poll::Ready(Some(TreeDiffEntry {
                        path: next.path,
                        values: Ok((before, Merge::absent())),
                    }));
                }
                _ => {
                    return Poll::Ready(Some(next));
                }
            }
        }
        Poll::Ready(self.held_file.take())
    }
}

/// Helper for writing trees with conflicts.
///
/// You start by creating an instance of this type with one or more
/// base trees. You then add overrides on top. The overrides may be
/// conflicts. Then you can write the result as a legacy tree
/// (allowing path-level conflicts) or as multiple conflict-free
/// trees.
pub struct MergedTreeBuilder {
    base_tree_id: MergedTreeId,
    overrides: BTreeMap<RepoPathBuf, MergedTreeValue>,
}

impl MergedTreeBuilder {
    /// Create a new builder with the given trees as base.
    pub fn new(base_tree_id: MergedTreeId) -> Self {
        Self {
            base_tree_id,
            overrides: BTreeMap::new(),
        }
    }

    /// Set an override compared to  the base tree. The `values` merge must
    /// either be resolved (i.e. have 1 side) or have the same number of
    /// sides as the `base_tree_ids` used to construct this builder. Use
    /// `Merge::absent()` to remove a value from the tree. When the base tree is
    /// a legacy tree, conflicts can be written either as a multi-way `Merge`
    /// value or as a resolved `Merge` value using `TreeValue::Conflict`.
    pub fn set_or_remove(&mut self, path: RepoPathBuf, values: MergedTreeValue) {
        if let MergedTreeId::Merge(_) = &self.base_tree_id {
            assert!(
                !values
                    .iter()
                    .flatten()
                    .any(|value| matches!(value, TreeValue::Conflict(_)))
            );
        }
        self.overrides.insert(path, values);
    }

    /// Create new tree(s) from the base tree(s) and overrides.
    pub fn write_tree(self, store: &Arc<Store>) -> BackendResult<MergedTreeId> {
        let base_tree_ids = match self.base_tree_id.clone() {
            MergedTreeId::Legacy(base_tree_id) => {
                let legacy_base_tree = store.get_tree(RepoPathBuf::root(), &base_tree_id)?;
                let base_tree = MergedTree::from_legacy_tree(legacy_base_tree)?;
                base_tree.id().to_merge()
            }
            MergedTreeId::Merge(base_tree_ids) => base_tree_ids,
        };
        let new_tree_ids = self.write_merged_trees(base_tree_ids, store)?;
        match new_tree_ids.simplify().into_resolved() {
            Ok(single_tree_id) => Ok(MergedTreeId::resolved(single_tree_id)),
            Err(tree_id) => {
                let tree = store.get_root_tree(&MergedTreeId::Merge(tree_id))?;
                let resolved = tree.resolve().block_on()?;
                Ok(resolved.id())
            }
        }
    }

    fn write_merged_trees(
        self,
        mut base_tree_ids: Merge<TreeId>,
        store: &Arc<Store>,
    ) -> BackendResult<Merge<TreeId>> {
        let num_sides = self
            .overrides
            .values()
            .map(|value| value.num_sides())
            .max()
            .unwrap_or(0);
        base_tree_ids.pad_to(num_sides, store.empty_tree_id());
        // Create a single-tree builder for each base tree
        let mut tree_builders =
            base_tree_ids.map(|base_tree_id| TreeBuilder::new(store.clone(), base_tree_id.clone()));
        for (path, values) in self.overrides {
            match values.into_resolved() {
                Ok(value) => {
                    // This path was overridden with a resolved value. Apply that to all
                    // builders.
                    for builder in tree_builders.iter_mut() {
                        builder.set_or_remove(path.clone(), value.clone());
                    }
                }
                Err(mut values) => {
                    values.pad_to(num_sides, &None);
                    // This path was overridden with a conflicted value. Apply each term to
                    // its corresponding builder.
                    for (builder, value) in zip(tree_builders.iter_mut(), values) {
                        builder.set_or_remove(path.clone(), value);
                    }
                }
            }
        }
        // TODO: This can be made more efficient. If there's a single resolved conflict
        // in `dir/file`, we shouldn't have to write the `dir/` and root trees more than
        // once.
        let merge_builder: MergeBuilder<TreeId> = tree_builders
            .into_iter()
            .map(|builder| builder.write_tree())
            .try_collect()?;
        Ok(merge_builder.build())
    }
}
