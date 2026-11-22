// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

unsafe extern "C" {
    #[link_name = "x86_secondary_cpu_long_mode_entry"]
    pub(super) static X86_SECONDARY_CPU_LONG_MODE_ENTRY: u8;

}
