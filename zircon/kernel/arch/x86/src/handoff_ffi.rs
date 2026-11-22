// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

unsafe extern "C" {
    /// Loads the kernel's early boot GDT (`load_startup_gdt` in `gdt.S`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that this function is called at CPL 0 during early bootstrap.
    pub fn load_startup_gdt();

    /// Loads the early boot IDT (`_idt_startup`) into `IDTR` (`load_startup_idt` in `asm.S`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that `_idt_startup` has been initialized and that this function is
    /// called at CPL 0 during early bootstrap.
    pub fn load_startup_idt();

    /// Populates the boot CPUID leaf cache (`InitializeBootCpuid` in `boot-cpuid.S`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that this function is called during single-threaded early boot on
    /// CPU 0 before concurrent readers access the boot CPUID cache.
    pub fn InitializeBootCpuid();
}
