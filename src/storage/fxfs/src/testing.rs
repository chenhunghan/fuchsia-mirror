// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::filesystem::FxFilesystem;
use crate::hooks::Hooks;
use fuchsia_async::Task;
use fuchsia_sync::Mutex;
use futures::FutureExt as _;
use futures::lock::MutexGuard;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub mod fake_object;
pub mod writer;

/// Guard returned by [`block_nth_commit`].  Dropping it clears the `before_commit` hook and
/// releases any acquired commit lock.
pub struct BlockCommitGuard<'a, 'h> {
    hooks: &'h mut Hooks<'a>,
    held: Arc<Mutex<Option<MutexGuard<'a, ()>>>>,
}

impl Drop for BlockCommitGuard<'_, '_> {
    fn drop(&mut self) {
        self.hooks.set_before_commit(|| {});
        self.held.lock().take();
    }
}

/// Configures `hooks` to block the `n`th (1-based) transaction commit on `fs` by acquiring
/// `fs.lock_commits()` inside the `before_commit` hook.  Dropping the returned guard clears the
/// hook and releases the commit lock.
pub fn block_nth_commit<'a, 'h>(
    hooks: &'h mut Hooks<'a>,
    fs: &'a FxFilesystem,
    n: usize,
) -> BlockCommitGuard<'a, 'h> {
    assert!(n >= 1);
    let count = AtomicUsize::new(0);
    let held: Arc<Mutex<Option<MutexGuard<'a, ()>>>> = Arc::new(Mutex::new(None));
    let held_clone = held.clone();
    hooks.set_before_commit(move || {
        if count.fetch_add(1, Ordering::SeqCst) + 1 == n {
            *held_clone.lock() = Some(fs.lock_commits().now_or_never().unwrap());
        }
    });
    BlockCommitGuard { hooks, held }
}

// The executor won't immediately wake threads to run tasks in parallel.  This
// is a hack that should force the executor to use multiple threads to run
// tasks.  Returns a vector of tasks that ensures the threads continue running.
pub async fn force_executor_threads_to_run(num_threads: usize) -> Vec<Task<()>> {
    let mut tasks = Vec::new();

    // Spawn tasks that continually yield.
    for _ in 1..num_threads {
        tasks.push(Task::spawn(async {
            loop {
                fuchsia_async::yield_now().await;
            }
        }));
    }

    // Loop for a while to make sure the threads are running before we return.
    for _ in 0..32 * num_threads {
        fuchsia_async::yield_now().await;
    }

    tasks
}
