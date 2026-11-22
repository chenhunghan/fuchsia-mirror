// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! KASAN early and late boot initialization hooks.

use init::{LK_INIT_LEVEL_VM, LK_INIT_LEVEL_VM_PREHEAP, LkInitLevel, lk_init_hook};

use crate::vm::pmm;

use super::arch::{arch_asan_early_init, arch_asan_late_init};
use super::asan_stubs::asan_register_globals_late;

#[sanitize(address = "off")]
fn asan_early_init(_arg: LkInitLevel) {
    // SAFETY: Invoked once at `LK_INIT_LEVEL_VM_PREHEAP` after the PMM is initialized.
    unsafe {
        arch_asan_early_init();
    }
}

#[sanitize(address = "off")]
fn asan_late_init(_arg: LkInitLevel) {
    // SAFETY: Invoked once at `LK_INIT_LEVEL_VM` after the VM and kernel aspace are initialized.
    unsafe {
        arch_asan_late_init();
        pmm::node().poison_all_free_pages();

        asan_register_globals_late();
    }
}

lk_init_hook!(asan_early_init, asan_early_init, LK_INIT_LEVEL_VM_PREHEAP);
lk_init_hook!(asan_late_init, asan_late_init, LK_INIT_LEVEL_VM);
