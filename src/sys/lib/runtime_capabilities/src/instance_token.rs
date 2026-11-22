// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use smallbox::SmallBox;
use smallbox::space::S2;
use std::any::Any;
use std::fmt::Debug;
use std::sync::{Arc, LazyLock};

/// The trait that `WeakInstanceToken` holds.
pub trait WeakInstanceTokenAny: Debug + Send + Sync {
    fn as_any(&self) -> &dyn Any;
}

/// A type representing a weak pointer to a component.
/// This is type erased because the bedrock library shouldn't depend on
/// Component Manager types.
#[derive(Debug)]
pub struct WeakInstanceToken {
    pub inner: SmallBox<dyn WeakInstanceTokenAny, S2>,
}

impl WeakInstanceToken {
    pub fn new<T: WeakInstanceTokenAny + 'static>(inner: T) -> Arc<Self> {
        Arc::new(Self { inner: smallbox::smallbox!(inner) })
    }

    /// Creates a new WeakInstanceToken that cannot be typecast into anything useful. Primarily
    /// useful in tests.
    pub fn new_invalid() -> Arc<Self> {
        #[derive(Debug)]
        struct Nothing;
        impl WeakInstanceTokenAny for Nothing {
            fn as_any(&self) -> &dyn Any {
                self
            }
        }
        static INVALID: LazyLock<Arc<WeakInstanceToken>> =
            LazyLock::new(|| WeakInstanceToken::new(Nothing {}));
        Arc::clone(&INVALID)
    }

    #[cfg(target_os = "fuchsia")]
    pub fn try_into_directory_entry(
        self: Arc<Self>,
        _scope: vfs::execution_scope::ExecutionScope,
        _token: Arc<crate::WeakInstanceToken>,
    ) -> Result<Arc<dyn vfs::directory::entry::DirectoryEntry>, crate::ConversionError> {
        Err(crate::ConversionError::NotSupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    struct InlineToken([usize; 2]);

    impl WeakInstanceTokenAny for InlineToken {
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn inline_storage_and_downcast() {
        let token = WeakInstanceToken::new(InlineToken([10, 20]));
        assert!(!token.inner.is_heap());
        assert_eq!(
            token.inner.as_any().downcast_ref::<InlineToken>(),
            Some(&InlineToken([10, 20]))
        );
    }
}
