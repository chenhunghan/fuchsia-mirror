// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::atomic_stack::{StackNode, StackNodeHeader};
use std::ptr::NonNull;

/// What to do with the closure stored in a [`CallbackNode`] before freeing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CallbackAction {
    /// Run the closure, then free the node.
    Run,
    /// Drop the closure without running it, then free the node.
    Discard,
}

/// The type-erased header of a [`CallbackNode`].
#[repr(C)]
struct CallbackNodeHeader {
    /// Must be the first field so that a pointer to a `CallbackNodeHeader` is also a valid
    /// pointer to its `StackNodeHeader`.
    stack_header: StackNodeHeader,

    /// Runs the closure (if `action` is `CallbackAction::Run`) and then drops it, freeing the
    /// whole `CallbackNode`.
    invoke_or_drop: unsafe fn(*mut CallbackNodeHeader, action: CallbackAction),
}

/// A heap-allocated callback, with the header placed at offset 0.
#[repr(C)]
struct CallbackNode<F> {
    header: CallbackNodeHeader,
    callback: F,
}

impl<F: FnOnce() + Send + Sync + 'static> CallbackNode<F> {
    /// # Safety
    /// `ptr` must point to the `header` of a `CallbackNode<F>` allocated by `RcuCallback::new`,
    /// with provenance over the whole allocation, and the caller must own that allocation. On
    /// return the allocation has been freed and `ptr` must not be used again.
    unsafe fn invoke_or_drop(ptr: *mut CallbackNodeHeader, action: CallbackAction) {
        // SAFETY: `header` is at offset 0 of the `#[repr(C)]` `CallbackNode<F>` and `ptr` has
        // provenance over the whole `Box<CallbackNode<F>>` allocation, so casting back and
        // reconstructing the box is sound. The caller transfers ownership to us.
        let node = unsafe { Box::from_raw(ptr.cast::<CallbackNode<F>>()) };
        match action {
            CallbackAction::Run => (node.callback)(),
            CallbackAction::Discard => {}
        }
    }
}

/// A single-allocation type-erased callback that can be pushed onto an
/// [`AtomicStack`](crate::atomic_stack::AtomicStack).
pub(crate) struct RcuCallback {
    /// Owning pointer to the header of a `CallbackNode<F>` created by `RcuCallback::new`.
    ptr: NonNull<CallbackNodeHeader>,
}

// SAFETY: `RcuCallback` uniquely owns a closure that is `Send`, as required by `RcuCallback::new`,
// so moving it to another thread is sound.
unsafe impl Send for RcuCallback {}

// SAFETY: `RcuCallback` has no methods that take `&self` (`invoke` consumes `self`), so a shared
// reference can't be used to access the underlying closure from multiple threads.
unsafe impl Sync for RcuCallback {}

impl RcuCallback {
    pub(crate) fn new<F>(callback: F) -> Self
    where
        F: FnOnce() + Send + Sync + 'static,
    {
        let node = Box::new(CallbackNode {
            header: CallbackNodeHeader {
                stack_header: StackNodeHeader::new(),
                invoke_or_drop: CallbackNode::<F>::invoke_or_drop,
            },
            callback,
        });
        // Cast (rather than reborrowing the `header` field) so the pointer keeps provenance over
        // the whole allocation.
        let ptr = Box::into_raw(node).cast::<CallbackNodeHeader>();
        // SAFETY: `Box::into_raw` never returns a null pointer.
        Self { ptr: unsafe { NonNull::new_unchecked(ptr) } }
    }

    /// Runs the callback, consuming it.
    pub(crate) fn invoke(self) {
        let ptr = self.ptr.as_ptr();
        // Forget `self` before running the callback so that `Drop` does not free the node again,
        // even if the callback panics.
        std::mem::forget(self);
        // SAFETY: `ptr` came from `RcuCallback::new` and we now uniquely own it because `self`
        // was forgotten. `invoke_or_drop` was set to `CallbackNode::<F>::invoke_or_drop` for the
        // matching `F`, so its contract holds.
        unsafe {
            ((*ptr).invoke_or_drop)(ptr, CallbackAction::Run);
        }
    }
}

impl Drop for RcuCallback {
    fn drop(&mut self) {
        let ptr = self.ptr.as_ptr();
        // SAFETY: `ptr` came from `RcuCallback::new` and is uniquely owned by `self`, which is
        // being dropped. `invoke_or_drop` matches the node's `F`. Passing
        // `CallbackAction::Discard` drops the closure and frees the node without running the
        // callback.
        unsafe {
            ((*ptr).invoke_or_drop)(ptr, CallbackAction::Discard);
        }
    }
}

// SAFETY:
// - `into_raw` returns the non-null pointer from `Box::into_raw`, cast to the
//   `CallbackNodeHeader` at offset 0. Its first field is the `StackNodeHeader`, so the result is
//   a valid pointer to a `StackNodeHeader` with provenance over the whole allocation.
// - `RcuCallback` never touches `stack_header`, so the stack has exclusive access to it while the
//   node is on the stack.
// - `from_raw` reconstructs the `RcuCallback` that owns the same allocation.
unsafe impl StackNode for RcuCallback {
    fn into_raw(self) -> *mut StackNodeHeader {
        let ptr = self.ptr.as_ptr().cast::<StackNodeHeader>();
        // Ownership of the allocation moves to the returned pointer.
        std::mem::forget(self);
        ptr
    }

    unsafe fn from_raw(ptr: *mut StackNodeHeader) -> Self {
        // SAFETY: The caller guarantees `ptr` was returned by `RcuCallback::into_raw`, so it is
        // non-null and points to the `CallbackNodeHeader` at offset 0 of a live `CallbackNode`.
        Self { ptr: unsafe { NonNull::new_unchecked(ptr.cast::<CallbackNodeHeader>()) } }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atomic_stack::AtomicStack;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DropCounter(Arc<AtomicUsize>);

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn test_rcu_callback_invoke_and_drop() {
        let run_counter = Arc::new(AtomicUsize::new(0));
        let drop_counter = Arc::new(AtomicUsize::new(0));

        let list = AtomicStack::<RcuCallback>::new();
        let run_clone = run_counter.clone();
        let captured_1 = DropCounter(drop_counter.clone());
        list.push_front(RcuCallback::new(move || {
            let _ = &captured_1;
            run_clone.fetch_add(1, Ordering::Relaxed);
        }));

        for cb in list.take() {
            cb.invoke();
        }
        assert_eq!(run_counter.load(Ordering::Relaxed), 1);
        assert_eq!(drop_counter.load(Ordering::Relaxed), 1);

        // Verify dropping a stack of RcuCallbacks drops captured state without invoking.
        let run_clone = run_counter.clone();
        let captured_2 = DropCounter(drop_counter.clone());
        list.push_front(RcuCallback::new(move || {
            let _ = &captured_2;
            run_clone.fetch_add(1, Ordering::Relaxed);
        }));
        drop(list);
        assert_eq!(run_counter.load(Ordering::Relaxed), 1);
        assert_eq!(drop_counter.load(Ordering::Relaxed), 2);
    }
}
