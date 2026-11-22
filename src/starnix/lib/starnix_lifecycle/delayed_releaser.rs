// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_types::ownership::{Releasable, ReleaseGuard};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ops::Deref;

pub trait ReleaserAction<T> {
    fn release(t: ReleaseGuard<T>);
}

/// Wrapper around `FileObject` that ensures that a unused `FileObject` is added to the current
/// delayed releasers to be released at the next release point.
pub struct ObjectReleaser<T, F: ReleaserAction<T>>(ManuallyDrop<ReleaseGuard<T>>, PhantomData<F>);

impl<T: Releasable, F: ReleaserAction<T>> Releasable for ObjectReleaser<T, F> {
    type Context<'a> = T::Context<'a>;

    fn release<'a>(mut self, context: Self::Context<'a>) {
        // SAFETY: `self` is forgotten immediately after taking `self.0`, so `Drop::drop` will
        // not run and `self.0` is taken exactly once.
        let object = unsafe { ManuallyDrop::take(&mut self.0) };
        std::mem::forget(self);
        object.release(context);
    }
}

impl<T: Default, F: ReleaserAction<T>> Default for ObjectReleaser<T, F> {
    fn default() -> Self {
        Self::from(T::default())
    }
}

impl<T, F: ReleaserAction<T>> From<T> for ObjectReleaser<T, F> {
    fn from(object: T) -> Self {
        Self(ManuallyDrop::new(object.into()), Default::default())
    }
}

impl<T, F: ReleaserAction<T>> Drop for ObjectReleaser<T, F> {
    fn drop(&mut self) {
        // SAFETY: The `ManuallyDrop` is only ever extracted here or in `Releasable::release`
        // (which forgets `self`), so it is guaranteed that it still exists.
        let object = unsafe { ManuallyDrop::take(&mut self.0) };
        F::release(object);
    }
}

impl<T: std::fmt::Debug, F: ReleaserAction<T>> std::fmt::Debug for ObjectReleaser<T, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.deref().fmt(f)
    }
}

impl<T, F: ReleaserAction<T>> std::ops::Deref for ObjectReleaser<T, F> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0.deref()
    }
}

impl<T, F: ReleaserAction<T>> std::borrow::Borrow<T> for ObjectReleaser<T, F> {
    fn borrow(&self) -> &T {
        self.deref()
    }
}

impl<T, F: ReleaserAction<T>> std::convert::AsRef<T> for ObjectReleaser<T, F> {
    fn as_ref(&self) -> &T {
        self.deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[derive(Default)]
    struct ReleaseCounts {
        direct_releases: Cell<usize>,
        action_releases: Cell<usize>,
    }

    struct TestObject<'a>(&'a ReleaseCounts);

    impl Releasable for TestObject<'_> {
        type Context<'a> = ();

        fn release<'a>(self, _context: Self::Context<'a>) {
            self.0.direct_releases.set(self.0.direct_releases.get() + 1);
        }
    }

    enum TestReleaserAction {}

    impl<'a> ReleaserAction<TestObject<'a>> for TestReleaserAction {
        fn release(guard: ReleaseGuard<TestObject<'a>>) {
            let object = ReleaseGuard::take(guard);
            object.0.action_releases.set(object.0.action_releases.get() + 1);
        }
    }

    type TestReleaser<'a> = ObjectReleaser<TestObject<'a>, TestReleaserAction>;

    #[::fuchsia::test]
    fn test_object_releaser_direct_release() {
        let counts = ReleaseCounts::default();
        let releaser = TestReleaser::from(TestObject(&counts));
        releaser.release(());
        assert_eq!(counts.direct_releases.get(), 1);
        assert_eq!(counts.action_releases.get(), 0);
    }

    #[::fuchsia::test]
    fn test_object_releaser_drop_runs_action() {
        let counts = ReleaseCounts::default();
        let releaser = TestReleaser::from(TestObject(&counts));
        drop(releaser);
        assert_eq!(counts.direct_releases.get(), 0);
        assert_eq!(counts.action_releases.get(), 1);
    }
}
