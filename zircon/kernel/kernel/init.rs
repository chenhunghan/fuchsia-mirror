// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2013 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::mp;
use crate::kernel::percpu::PerCpu;

/// Called at `LK_INIT_LEVEL_KERNEL`.
///
/// # Safety
///
/// Must only be called once on the boot processor (`cpu_num == 0`) with interrupts disabled
/// (`lk_main` before `thread::current_become_idle()`) after `PerCpu::initialize_boot`, kernel
/// heap initialization, and `CpuDistanceMap::Initialize` have succeeded, and before any
/// secondary processors are started.
pub unsafe fn kernel_init() {
    mp::init();

    // Allocate secondary percpu instances before booting other processors, after
    // vm and system topology are initialized.
    // SAFETY: The caller guarantees single-call execution on the boot processor (`cpu_num == 0`)
    // with interrupts disabled after `PerCpu::initialize_boot`, kernel heap initialization, and
    // `CpuDistanceMap::Initialize` have succeeded, and before any secondary processors start;
    // `mp::init()` above preserves all of these preconditions.
    unsafe {
        PerCpu::initialize_secondaries_begin();
    }
}
