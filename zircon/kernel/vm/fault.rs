// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// page fault flags
pub mod flag {
    use vm_fault_bindings as bindings;

    pub const WRITE: u32 = bindings::VMM_PF_FLAG_WRITE;
    /// Indicates the fault occurred while the CPU was executing in user mode.
    pub const USER: u32 = bindings::VMM_PF_FLAG_USER;
    pub const GUEST: u32 = bindings::VMM_PF_FLAG_GUEST;
    pub const INSTRUCTION: u32 = bindings::VMM_PF_FLAG_INSTRUCTION;
    pub const NOT_PRESENT: u32 = bindings::VMM_PF_FLAG_NOT_PRESENT;
    /// hardware is requesting a fault
    pub const HW_FAULT: u32 = bindings::VMM_PF_FLAG_HW_FAULT;
    /// software fault
    pub const SW_FAULT: u32 = bindings::VMM_PF_FLAG_SW_FAULT;
    pub const ACCESS: u32 = bindings::VMM_PF_FLAG_ACCESS;
    pub const FAULT_MASK: u32 = bindings::VMM_PF_FLAG_FAULT_MASK;
}

/// Convenience routine for converting page fault flags to a 4-character string.
#[inline]
pub fn flags_to_string(pf_flags: u32) -> &'static str {
    const FLAGS_MASK: u32 =
        flag::WRITE | flag::USER | flag::GUEST | flag::INSTRUCTION | flag::NOT_PRESENT;
    zr::static_assert!(FLAGS_MASK == 0x1f);

    const TABLE: [[u8; 4]; 32] = {
        let mut table = [[0u8; 4]; 32];
        let mut i = 0;
        while i < 32 {
            let f = i as u32;
            let bytes = [
                if (f & flag::WRITE) != 0 { b'w' } else { b'r' },
                if (f & flag::USER) != 0 {
                    b'u'
                } else if (f & flag::GUEST) != 0 {
                    b'g'
                } else {
                    b's'
                },
                if (f & flag::INSTRUCTION) != 0 { b'i' } else { b'd' },
                if (f & flag::NOT_PRESENT) != 0 { b'n' } else { b'p' },
            ];
            assert!(bytes.is_ascii());
            table[i] = bytes;
            i += 1;
        }
        table
    };

    // SAFETY: Every entry in `TABLE` is verified to be ASCII (`bytes.is_ascii()`) at compile time,
    // and `(pf_flags & FLAGS_MASK) as usize` is in `0..32` (since `FLAGS_MASK == 0x1f`).
    unsafe { core::str::from_utf8_unchecked(&TABLE[(pf_flags & FLAGS_MASK) as usize]) }
}
