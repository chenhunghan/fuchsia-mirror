// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::writer::{Error, Node, State};
use derivative::Derivative;
use inspect_format::BlockIndex;
use private::InspectTypeInternal;
use std::borrow::Cow;
use std::fmt::Debug;
use std::sync::{Arc, Weak};

/// Trait implemented by all inspect types.
pub trait InspectType: Send + Sync + Debug {
    fn into_recorded(self) -> crate::writer::types::RecordedInspectType
    where
        Self: Sized + 'static;
}

pub(crate) mod private {
    use crate::writer::State;
    use inspect_format::BlockIndex;

    /// Trait implemented by all inspect types. It provides functions that are not
    /// intended for use outside the crate.
    /// Use `impl_inspect_type_internal` for easy implementation.
    pub trait InspectTypeInternal {
        fn is_valid(&self) -> bool;
        fn block_index(&self) -> Option<BlockIndex>;
        fn state(&self) -> Option<State>;
        fn atomic_access<R, F: FnOnce(&Self) -> R>(&self, accessor: F) -> R;
    }
}

/// Trait allowing a `Node` to adopt any Inspect type as its child, removing
/// it from the original parent's tree.
///
/// This trait is not implementable by external types.
pub trait InspectTypeReparentable: private::InspectTypeInternal {
    #[doc(hidden)]
    /// This function is called by a child with the new parent as an argument.
    /// The child will be removed from its current parent and added to the tree
    /// under new_parent.
    fn reparent(&self, new_parent: &Node) -> Result<(), Error> {
        if let (
            Some(child_state),
            Some(child_index),
            Some(new_parent_state),
            Some(new_parent_index),
        ) = (self.state(), self.block_index(), new_parent.state(), new_parent.block_index())
        {
            if new_parent_state != child_state {
                return Err(Error::AdoptionIntoWrongVmo);
            }

            new_parent_state
                .try_lock()
                .and_then(|mut state| state.reparent(child_index, new_parent_index))?;
        }

        Ok(())
    }
}

impl<T: private::InspectTypeInternal> InspectTypeReparentable for T {}

/// Trait allowing an Inspect type to be renamed.
///
/// This trait is not implementable by external types.
pub trait InspectTypeRenameable: private::InspectTypeInternal {
    /// Rename this inspect node or property.
    fn rename<'a>(&self, name: impl Into<Cow<'a, str>>) -> Result<(), Error>;
}

/// Macro to generate private::InspectTypeInternal
macro_rules! impl_inspect_type_internal {
    ($type_name:ident) => {
        impl $type_name {
            pub(crate) fn new(
                state: $crate::writer::State,
                block_index: inspect_format::BlockIndex,
            ) -> $type_name {
                $type_name { inner: $crate::writer::types::base::Inner::new(state, block_index) }
            }

            pub(crate) fn new_no_op() -> $type_name {
                $type_name { inner: $crate::writer::types::base::Inner::None }
            }

            /// Rename this inspect node or property.
            pub fn rename<'a>(
                &self,
                name: impl Into<std::borrow::Cow<'a, str>>,
            ) -> Result<(), $crate::writer::Error> {
                <Self as $crate::writer::InspectTypeRenameable>::rename(self, name)
            }
        }

        impl $crate::writer::InspectTypeRenameable for $type_name {
            fn rename<'a>(
                &self,
                name: impl Into<std::borrow::Cow<'a, str>>,
            ) -> Result<(), $crate::writer::Error> {
                self.inner.rename(name)
            }
        }

        impl $crate::private::InspectTypeInternal for $type_name {
            fn is_valid(&self) -> bool {
                self.inner.is_valid()
            }

            fn state(&self) -> Option<$crate::writer::State> {
                Some(self.inner.inner_ref()?.state.clone())
            }

            fn block_index(&self) -> Option<inspect_format::BlockIndex> {
                if let Some(ref inner_ref) = self.inner.inner_ref() {
                    Some(inner_ref.block_index)
                } else {
                    None
                }
            }

            fn atomic_access<R, F: FnOnce(&Self) -> R>(&self, accessor: F) -> R {
                match self.inner.inner_ref() {
                    None => {
                        // If the node was a no-op we still execute the `accessor` even if all
                        // operations inside it will be no-ops to return `R`.
                        accessor(&self)
                    }
                    Some(inner_ref) => {
                        // Silently ignore the error when fail to lock (as in any regular operation).
                        // All operations performed in the `accessor` won't update the vmo
                        // generation count since we'll be holding one lock here.
                        inner_ref.state.begin_transaction();
                        let result = accessor(&self);
                        inner_ref.state.end_transaction();
                        result
                    }
                }
            }
        }
    };
}

pub(crate) use impl_inspect_type_internal;

macro_rules! impl_inspect_type_internal_histogram {
    ($type_name:ident) => {
        impl $type_name {
            /// Rename this inspect histogram property.
            pub fn rename<'a>(
                &self,
                name: impl Into<std::borrow::Cow<'a, str>>,
            ) -> Result<(), $crate::writer::Error> {
                <Self as $crate::writer::InspectTypeRenameable>::rename(self, name)
            }
        }

        impl $crate::writer::InspectTypeRenameable for $type_name {
            fn rename<'a>(
                &self,
                name: impl Into<std::borrow::Cow<'a, str>>,
            ) -> Result<(), $crate::writer::Error> {
                self.array.rename(name)
            }
        }

        impl $crate::private::InspectTypeInternal for $type_name {
            fn is_valid(&self) -> bool {
                self.array.is_valid()
            }

            fn state(&self) -> Option<$crate::writer::State> {
                self.array.state()
            }

            fn block_index(&self) -> Option<inspect_format::BlockIndex> {
                self.array.block_index()
            }

            fn atomic_access<R, F: FnOnce(&Self) -> R>(&self, accessor: F) -> R {
                self.array.atomic_access(|_| accessor(self))
            }
        }
    };
}

pub(crate) use impl_inspect_type_internal_histogram;

/// An inner type of all inspect nodes and properties. Each variant implies a
/// different relationship with the underlying inspect VMO.
#[derive(Debug, Derivative)]
#[derivative(Default)]
pub(crate) enum Inner<T: InnerType> {
    /// The node or property is not attached to the inspect VMO.
    #[derivative(Default)]
    None,

    /// The node or property is attached to the inspect VMO, iff its strong
    /// reference is still alive.
    Weak(Weak<InnerRef<T>>),

    /// The node or property is attached to the inspect VMO.
    Strong(Arc<InnerRef<T>>),
}

impl<T: InnerType> Inner<T> {
    /// Creates a new Inner with the desired block index within the inspect VMO
    pub(crate) fn new(state: State, block_index: BlockIndex) -> Self {
        Self::Strong(Arc::new(InnerRef { state, block_index, data: T::Data::default() }))
    }

    pub(crate) fn rename<'a>(&self, name: impl Into<Cow<'a, str>>) -> Result<(), Error> {
        if let Some(inner_ref) = self.inner_ref() {
            let mut state = inner_ref.state.try_lock()?;
            if inner_ref.data.is_valid() {
                state.set_name(inner_ref.block_index, name)?;
            }
        }
        Ok(())
    }

    /// Returns true if the number of strong references to this node or property
    /// is greater than 0.
    pub(crate) fn is_valid(&self) -> bool {
        match self {
            Self::None => false,
            Self::Weak(weak_ref) => match weak_ref.upgrade() {
                None => false,
                Some(inner_ref) => inner_ref.data.is_valid(),
            },
            Self::Strong(inner_ref) => inner_ref.data.is_valid(),
        }
    }

    /// Returns a `Some(Arc<InnerRef>)` iff the node or property is currently
    /// attached to inspect, or `None` otherwise. Weak pointers are upgraded
    /// if possible, but their lifetime as strong references are expected to be
    /// short.
    pub(crate) fn inner_ref(&self) -> Option<Arc<InnerRef<T>>> {
        match self {
            Self::None => None,
            Self::Weak(weak_ref) => {
                if let Some(inner_ref) = weak_ref.upgrade()
                    && inner_ref.data.is_valid()
                {
                    return Some(inner_ref);
                }
                None
            }
            Self::Strong(inner_ref) => {
                if inner_ref.data.is_valid() {
                    Some(Arc::clone(inner_ref))
                } else {
                    None
                }
            }
        }
    }

    /// Make a weak reference.
    pub(crate) fn clone_weak(&self) -> Self {
        match self {
            Self::None => Self::None,
            Self::Weak(weak_ref) => Self::Weak(weak_ref.clone()),
            Self::Strong(inner_ref) => {
                if inner_ref.data.is_valid() {
                    Self::Weak(Arc::downgrade(inner_ref))
                } else {
                    Self::None
                }
            }
        }
    }
}

/// Inspect API types implement Eq,PartialEq returning true all the time so that
/// structs embedding inspect types can derive these traits as well.
/// IMPORTANT: Do not rely on these traits implementations for real comparisons
/// or validation tests, instead leverage the reader.
impl<T: InnerType> PartialEq for Inner<T> {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl<T: InnerType> Eq for Inner<T> {}

/// A type that is owned by inspect nodes and properties, sharing ownership of
/// the inspect VMO heap, and with numerical pointers to the location in the
/// heap in which it resides.
#[derive(Debug)]
pub(crate) struct InnerRef<T: InnerType> {
    /// Index of the block in the VMO.
    pub(crate) block_index: BlockIndex,

    /// Reference to the VMO heap.
    pub(crate) state: State,

    /// Associated data for this type.
    pub(crate) data: T::Data,
}

impl<T: InnerType> Drop for InnerRef<T> {
    /// InnerRef has a manual drop impl, to guarantee a single deallocation in
    /// the case of multiple strong references.
    fn drop(&mut self) {
        if let Err(e) = T::free(&self.state, &self.data, self.block_index) {
            log::error!("Failed to free InnerRef: {:?}", e);
        }
    }
}

/// De-allocation behavior and associated data for an inner type.
pub(crate) trait InnerType {
    /// Associated data stored on the InnerRef
    type Data: Default + Debug + InnerData;

    /// De-allocation behavior for when the InnerRef gets dropped
    fn free(state: &State, data: &Self::Data, block_index: BlockIndex) -> Result<(), Error>;
}

pub(crate) trait InnerData {
    fn is_valid(&self) -> bool;
}

impl InnerData for () {
    fn is_valid(&self) -> bool {
        true
    }
}

#[derive(Default, Debug)]
pub(crate) struct InnerValueType;

impl InnerType for InnerValueType {
    type Data = ();
    fn free(state: &State, _: &Self::Data, block_index: BlockIndex) -> Result<(), Error> {
        let mut state_lock = state.try_lock()?;
        state_lock.free_value(block_index).map_err(|err| Error::free("value", block_index, err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Inspector;
    use diagnostics_assertions::assert_data_tree;

    #[fuchsia::test]
    async fn test_reparent_from_state() {
        let insp = Inspector::default();
        let root = insp.root();
        let a = root.create_child("a");
        let b = a.create_child("b");

        assert_data_tree!(insp, root: {
            a: {
                b: {},
            },
        });

        b.reparent(root).unwrap();

        assert_data_tree!(insp, root: {
            b: {},
            a: {},
        });
    }

    #[fuchsia::test]
    fn reparent_from_wrong_state() {
        let insp1 = Inspector::default();
        let insp2 = Inspector::default();

        assert!(insp1.root().reparent(insp2.root()).is_err());

        let a = insp1.root().create_child("a");
        let b = insp2.root().create_child("b");

        assert!(a.reparent(&b).is_err());
        assert!(b.reparent(&a).is_err());
    }

    #[fuchsia::test]
    async fn test_rename_nodes_and_properties() {
        use crate::writer::{ArrayProperty, HistogramProperty};
        use diagnostics_hierarchy::{LinearHistogram, LinearHistogramParams};
        use futures::FutureExt;

        let insp = Inspector::default();
        let root = insp.root();
        let node = root.create_child("node");
        let int_prop = node.create_int("int_prop", 42);
        let str_prop = node.create_string("str_prop", "hello");
        let array_prop = node.create_int_array("array_prop", 2);
        array_prop.set(0, 1);
        array_prop.set(1, 2);
        let hist_prop = node.create_int_linear_histogram(
            "hist_prop",
            LinearHistogramParams { floor: 0, step_size: 10, buckets: 2 },
        );
        hist_prop.insert(5);
        let lazy_node = node.create_lazy_child("lazy_node", || {
            async move {
                let lazy_insp = Inspector::default();
                lazy_insp.root().record_int("val", 99);
                Ok(lazy_insp)
            }
            .boxed()
        });

        assert_data_tree!(insp, root: {
            node: {
                int_prop: 42i64,
                str_prop: "hello",
                array_prop: vec![1i64, 2i64],
                hist_prop: LinearHistogram {
                    floor: 0i64,
                    step: 10,
                    counts: vec![1],
                    indexes: Some(vec![1]),
                    size: 4,
                },
                lazy_node: {
                    val: 99i64,
                },
            },
        });

        node.rename("node_renamed").unwrap();
        int_prop.rename("int_renamed").unwrap();
        str_prop.rename("str_renamed").unwrap();
        array_prop.rename("array_renamed").unwrap();
        hist_prop.rename("hist_renamed").unwrap();
        lazy_node.rename("lazy_renamed").unwrap();

        assert_data_tree!(insp, root: {
            node_renamed: {
                int_renamed: 42i64,
                str_renamed: "hello",
                array_renamed: vec![1i64, 2i64],
                hist_renamed: LinearHistogram {
                    floor: 0i64,
                    step: 10,
                    counts: vec![1],
                    indexes: Some(vec![1]),
                    size: 4,
                },
                lazy_renamed: {
                    val: 99i64,
                },
            },
        });
    }

    #[fuchsia::test]
    async fn test_rename_root_and_noop_and_weak() {
        let insp = Inspector::default();
        assert_eq!(insp.root().rename("new_root"), Err(Error::RenameRoot));

        let noop_node = Node::default();
        assert_eq!(noop_node.rename("ignored"), Ok(()));

        let node = insp.root().create_child("node");
        let _child = node.create_child("child");
        let weak = node.clone_weak();
        weak.rename("weak_renamed").unwrap();
        assert_data_tree!(insp, root: {
            weak_renamed: {
                child: {},
            },
        });

        node.forget();
        assert_eq!(node.rename("after_forget"), Ok(()));
        assert_eq!(weak.rename("after_forget"), Ok(()));
    }

    #[fuchsia::test]
    fn test_rename_string_reference_lifecycle() {
        let insp = Inspector::default();
        let state = insp.state().unwrap();

        let node = insp.root().create_child("unique_old_name");
        let stats_before = state.try_lock().unwrap().stats();

        // Renaming to the same name should be a no-op with no block allocations/deallocations.
        node.rename("unique_old_name").unwrap();
        let stats_same = state.try_lock().unwrap().stats();
        assert_eq!(stats_before.allocated_blocks, stats_same.allocated_blocks);
        assert_eq!(stats_before.deallocated_blocks, stats_same.deallocated_blocks);

        // Renaming to a new unique name should allocate 1 new string ref block and deallocate the
        // old 1.
        node.rename("unique_new_name").unwrap();
        let stats_after = state.try_lock().unwrap().stats();
        assert_eq!(stats_after.allocated_blocks, stats_before.allocated_blocks + 1);
        assert_eq!(stats_after.deallocated_blocks, stats_before.deallocated_blocks + 1);
    }
}
