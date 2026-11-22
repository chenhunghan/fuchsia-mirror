// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use page;

// SAFETY: These symbols represent `.label` offsets in `.text.bootstrap16` and are not mutated.
unsafe extern "C" {
    #[link_name = "x86_bootstrap16_start"]
    pub(super) static X86_BOOTSTRAP16_START: [u8; page::SIZE];

    #[link_name = "x86_bootstrap16_end"]
    pub(super) static X86_BOOTSTRAP16_END: u8;

    #[link_name = "x86_secondary_cpu_long_mode_high_entry"]
    pub(super) static X86_SECONDARY_CPU_LONG_MODE_HIGH_ENTRY: u8;
}
