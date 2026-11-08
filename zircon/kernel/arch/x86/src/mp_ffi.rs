// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

unsafe extern "C" {
    /// Translates `apic_id` to its logical CPU number, or `-1` if not found.
    ///
    /// # Safety
    ///
    /// The caller must ensure that AP structures have been allocated.
    pub fn x86_apic_id_to_cpu_num(apic_id: u32) -> core::ffi::c_int;

    /// Returns a pointer to `cpu_num`'s `x86_percpu` structure.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `cpu_num < x86_num_cpus`.
    pub fn x86_percpu_for(
        cpu_num: crate::kernel::types::cpu_num_t,
    ) -> *mut crate::arch_rs::x86::handoff::X86Percpu;

    /// Initializes the per-CPU state for `cpu_num`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `%gs.base` points to `cpu_num`'s `x86_percpu` structure.
    pub fn x86_init_percpu(cpu_num: crate::kernel::types::cpu_num_t);

    /// Forcibly stops all other CPUs except the current one and the BSP (which is cpu 0).
    ///
    /// # Safety
    ///
    /// The caller must ensure that the system is panicking or halting.
    pub fn x86_force_halt_all_but_local_and_bsp();

    #[link_name = "bp_percpu"]
    pub static BP_PERCPU: crate::arch_rs::x86::handoff::X86Percpu;
}

#[cfg(console_enabled)]
pub mod console {
    use crate::arch_rs::arch_x86::idle_states::X86IdleStates;

    unsafe extern "C" {
        #[link_name = "use_monitor"]
        pub static mut USE_MONITOR: bool;

        #[link_name = "x86_num_cpus"]
        pub static mut X86_NUM_CPUS: u8;

        /// Returns a pointer to the BSP's idle states.
        ///
        /// # Safety
        ///
        /// The caller must ensure that `x86_allocate_ap_structures` has been called.
        #[link_name = "cpp_x86_mp_bp_idle_states"]
        pub fn x86_mp_bp_idle_states() -> *mut X86IdleStates;

        /// Returns a pointer to the AP's idle states for the given index.
        ///
        /// # Safety
        ///
        /// The caller must ensure that `idx < x86_num_cpus - 1` and `x86_allocate_ap_structures`
        /// has run.
        #[link_name = "cpp_x86_mp_ap_idle_states"]
        pub fn x86_mp_ap_idle_states(idx: usize) -> *mut X86IdleStates;
    }
}
