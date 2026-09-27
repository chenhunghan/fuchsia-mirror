// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::writer::{ArrayProperty, Inner, InnerValueType, InspectType};
use std::borrow::Cow;

#[derive(Debug, PartialEq, Eq, Default)]
pub struct StringArrayProperty {
    inner: Inner<InnerValueType>,
}

impl InspectType for StringArrayProperty {
    fn into_recorded(self) -> crate::writer::types::RecordedInspectType {
        crate::writer::types::RecordedInspectType::StringArray(self)
    }
}

crate::impl_inspect_type_internal!(StringArrayProperty);

impl ArrayProperty for StringArrayProperty {
    type Type<'a> = Cow<'a, str>;

    fn set<'a>(&self, index: usize, value: impl Into<Self::Type<'a>>) {
        if let Some(ref inner_ref) = self.inner.inner_ref() {
            inner_ref
                .state
                .try_lock()
                .and_then(|mut state| {
                    state.set_array_string_slot(inner_ref.block_index, index, value.into())
                })
                .ok();
        }
    }

    fn clear(&self) {
        if let Some(ref inner_ref) = self.inner.inner_ref() {
            inner_ref
                .state
                .try_lock()
                .and_then(|mut state| state.clear_array(inner_ref.block_index, 0))
                .ok();
        }
    }
}

impl Drop for StringArrayProperty {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::Length;
    use crate::writer::private::InspectTypeInternal;
    use crate::writer::testing_utils::GetBlockExt;
    use crate::{Inspector, assert_update_is_atomic};
    use diagnostics_assertions::assert_json_diff;

    impl StringArrayProperty {
        pub fn load_string_slot(&self, slot: usize) -> Option<String> {
            self.inner.inner_ref().and_then(|inner_ref| {
                inner_ref.state.try_lock().ok().and_then(|state| {
                    let index =
                        state.get_block(self.block_index().unwrap()).get_string_index_at(slot)?;
                    state.load_string(index).ok()
                })
            })
        }
    }

    #[fuchsia::test]
    async fn string_array_property() {
        let inspector = Inspector::default();
        let root = inspector.root();
        let node = root.create_child("node");

        {
            let array = node.create_string_array("string_array", 5);
            assert_eq!(array.len().unwrap(), 5);
            node.get_block::<_, inspect_format::Node>(|node_block| {
                assert_eq!(node_block.child_count(), 1);
            });

            array.set(0, "0");
            array.set(1, "1");
            array.set(2, "2");
            array.set(3, "3");
            array.set(4, "4");

            // this should fail silently
            array.set(5, "5");
            assert!(array.load_string_slot(5).is_none());

            let expected: Vec<String> =
                vec!["0".into(), "1".into(), "2".into(), "3".into(), "4".into()];

            assert_json_diff!(inspector, root: {
                node: {
                    string_array: expected,
                },
            });

            array.clear();

            let expected: Vec<String> = vec![String::new(); 5];

            assert_json_diff!(inspector, root: {
                node: {
                    string_array: expected,
                },
            });

            assert!(array.load_string_slot(5).is_none());
        }

        node.get_block::<_, inspect_format::Node>(|node_block| {
            assert_eq!(node_block.child_count(), 0);
        });
    }

    #[fuchsia::test]
    fn property_atomics() {
        let inspector = Inspector::default();
        let array = inspector.root().create_string_array("string_array", 5);

        assert_update_is_atomic!(array, |array| {
            array.set(0, "0");
            array.set(1, "1");
            array.set(2, "2");
            array.set(3, "3");
            array.set(4, "4");
        });
    }
}
