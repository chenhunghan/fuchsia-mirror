// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Register state saved and restored when entering and exiting a guest.
//!
//! TODO(https://fxbug.dev/576430432): Remove the C++ struct definitions in
//! `<arch/x86/hypervisor/vmx_state.h>` once vcpu.cc is ported to Rust. These mirror the layouts of
//! those C++ types, which `vmx.S` accesses at the offsets defined there.

use core::mem::{align_of, offset_of, size_of};
use guest_bindings as bindings;
use zr::static_assert;

use crate::kernel::bits::bits_shift;

/// Holds the register state used to restore a host.
#[repr(C)]
#[derive(Default)]
pub struct HostState {
    /// Host stack pointer.
    pub rsp: u64,

    /// Extended control registers.
    pub xcr0: u64,
}

/// Holds the register state of a guest that VMX does not save in the VMCS.
///
/// RIP, RSP, and RFLAGS are automatically saved by VMX in the VMCS.
#[repr(C)]
#[derive(Default)]
pub struct GuestState {
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rbx: u64,
    pub rbp: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,

    /// Control registers.
    pub cr2: u64,

    /// Extended control registers.
    pub xcr0: u64,
}

impl GuestState {
    // Convenience getters for accessing low 32-bits of common registers.

    /// Returns the low 32 bits of RAX.
    pub fn eax(&self) -> u32 {
        self.rax as u32
    }

    /// Returns the low 32 bits of RCX.
    pub fn ecx(&self) -> u32 {
        self.rcx as u32
    }

    /// Returns the low 32 bits of RDX.
    pub fn edx(&self) -> u32 {
        self.rdx as u32
    }

    /// Returns the low 32 bits of RBX.
    pub fn ebx(&self) -> u32 {
        self.rbx as u32
    }

    /// Returns the 64-bit value EDX:EAX, used by several x86_64 instructions, such as `rdmsr` and
    /// `wrmsr`.
    ///
    /// The top bits of RAX and RDX are ignored (c.f. Volume 2C, WRMSR).
    pub fn edx_eax(&self) -> u64 {
        (u64::from(self.edx()) << 32) | u64::from(self.eax())
    }

    /// Sets EDX:EAX to `value`, clearing the top bits of RAX and RDX to match the behavior of
    /// x86_64 instructions such as `rdmsr` (c.f. Volume 2C, RDMSR).
    pub fn set_edx_eax(&mut self, value: u64) {
        self.rax = bits_shift(value, 31, 0);
        self.rdx = bits_shift(value, 63, 32);
    }
}

/// Holds the register state saved and restored by `vmx_enter_asm`.
#[repr(C)]
#[derive(Default)]
pub struct VmxState {
    pub resume: bool,
    pub host_state: HostState,
    pub guest_state: GuestState,
}

static_assert!(size_of::<HostState>() == size_of::<bindings::HostState>());
static_assert!(align_of::<HostState>() == align_of::<bindings::HostState>());
static_assert!(size_of::<GuestState>() == size_of::<bindings::GuestState>());
static_assert!(align_of::<GuestState>() == align_of::<bindings::GuestState>());
static_assert!(size_of::<VmxState>() == size_of::<bindings::VmxState>());
static_assert!(align_of::<VmxState>() == align_of::<bindings::VmxState>());

/// Returns the offset of `$field` of `GuestState` within `VmxState`.
macro_rules! gs_offset {
    ($field:ident) => {
        offset_of!(VmxState, guest_state) + offset_of!(GuestState, $field)
    };
}

static_assert!(offset_of!(VmxState, resume) == bindings::VS_RESUME as usize);

static_assert!(
    offset_of!(VmxState, host_state) + offset_of!(HostState, rsp) == bindings::HS_RSP as usize
);
static_assert!(
    offset_of!(VmxState, host_state) + offset_of!(HostState, xcr0) == bindings::HS_XCR0 as usize
);

static_assert!(gs_offset!(rax) == bindings::GS_RAX as usize);
static_assert!(gs_offset!(rbx) == bindings::GS_RBX as usize);
static_assert!(gs_offset!(rcx) == bindings::GS_RCX as usize);
static_assert!(gs_offset!(rdx) == bindings::GS_RDX as usize);
static_assert!(gs_offset!(rdi) == bindings::GS_RDI as usize);
static_assert!(gs_offset!(rsi) == bindings::GS_RSI as usize);
static_assert!(gs_offset!(rbp) == bindings::GS_RBP as usize);
static_assert!(gs_offset!(r8) == bindings::GS_R8 as usize);
static_assert!(gs_offset!(r9) == bindings::GS_R9 as usize);
static_assert!(gs_offset!(r10) == bindings::GS_R10 as usize);
static_assert!(gs_offset!(r11) == bindings::GS_R11 as usize);
static_assert!(gs_offset!(r12) == bindings::GS_R12 as usize);
static_assert!(gs_offset!(r13) == bindings::GS_R13 as usize);
static_assert!(gs_offset!(r14) == bindings::GS_R14 as usize);
static_assert!(gs_offset!(r15) == bindings::GS_R15 as usize);
static_assert!(gs_offset!(cr2) == bindings::GS_CR2 as usize);
static_assert!(offset_of!(GuestState, xcr0) == offset_of!(bindings::GuestState, xcr0));
static_assert!(offset_of!(VmxState, guest_state) == offset_of!(bindings::VmxState, guest_state));
