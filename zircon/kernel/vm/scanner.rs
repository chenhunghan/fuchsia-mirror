// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use scanner_bindings as bindings;

/// RAII guard to disable the VM scanner for a scope, restoring the scanner state on drop.
pub struct AutoVmScannerDisable(());

impl AutoVmScannerDisable {
    /// Creates a new `AutoVmScannerDisable` guard, disabling the VM scanner.
    pub fn new() -> Self {
        unsafe { bindings::cpp_scanner_push_disable_count() };
        Self(())
    }
}

impl Drop for AutoVmScannerDisable {
    fn drop(&mut self) {
        unsafe { bindings::cpp_scanner_pop_disable_count() };
    }
}
