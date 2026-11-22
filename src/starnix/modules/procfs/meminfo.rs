// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_core::task::{CurrentTask, KernelStats};
use starnix_core::vfs::FsNodeOps;
use starnix_core::vfs::pseudo::dynamic_file::{DynamicFile, DynamicFileBuf, DynamicFileSource};
use starnix_logging::log_error;
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;
use std::sync::Arc;

#[derive(Clone)]
pub struct MeminfoFile {
    kernel_stats: Arc<KernelStats>,
}

impl MeminfoFile {
    pub fn new_node(kernel_stats: &Arc<KernelStats>) -> impl FsNodeOps {
        DynamicFile::new_node(Self { kernel_stats: kernel_stats.clone() })
    }
}

impl DynamicFileSource for MeminfoFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        let stats = self.kernel_stats.get();
        let memory_stats =
            stats.get_memory_stats_extended(zx::MonotonicInstant::INFINITE).map_err(|e| {
                log_error!("FIDL error getting memory stats: {e}");
                errno!(EIO)
            })?;
        let compression_stats =
            stats.get_memory_stats_compression(zx::MonotonicInstant::INFINITE).map_err(|e| {
                log_error!("FIDL error getting memory compression stats: {e}");
                errno!(EIO)
            })?;

        let mem_total = memory_stats.total_bytes.unwrap_or_default() / 1024;
        let mem_free = memory_stats.free_bytes.unwrap_or_default() / 1024;
        let mem_available = (memory_stats.free_bytes.unwrap_or_default()
            + memory_stats.vmo_pager_oldest_bytes.unwrap_or_default()
            + memory_stats.vmo_discardable_unlocked_bytes.unwrap_or_default())
            / 1024;

        let userpager_total = memory_stats.vmo_pager_total_bytes.unwrap_or_default() / 1024;
        let userpager_active = memory_stats.vmo_pager_newest_bytes.unwrap_or_default() / 1024;
        let userpager_inactive = userpager_total.saturating_sub(userpager_active);

        let anonymous_total = memory_stats
            .vmo_bytes
            .unwrap_or_default()
            .saturating_sub(memory_stats.vmo_pager_total_bytes.unwrap_or_default())
            / 1024;

        let swap_used = compression_stats.uncompressed_storage_bytes.unwrap_or_default() / 1024;
        // Fuchsia doesn't have a limit on the size of its swap file, but a common value is 75% of
        // the total memory.
        let swap_total = 3 * mem_total / 4;
        let swap_free = swap_total.saturating_sub(swap_used);

        sink.reserve(512);
        write!(
            sink,
            "MemTotal:       {mem_total:8} kB\n\
             MemFree:        {mem_free:8} kB\n\
             MemAvailable:   {mem_available:8} kB\n\
             Cached:         {userpager_total:8} kB\n\
             SwapTotal:      {swap_total:8} kB\n\
             SwapFree:       {swap_free:8} kB\n\
             Active(anon):   {anonymous_total:8} kB\n\
             Inactive(anon):        0 kB\n\
             Active(file):   {userpager_active:8} kB\n\
             Inactive(file): {userpager_inactive:8} kB\n",
        )?;

        Ok(())
    }
}
