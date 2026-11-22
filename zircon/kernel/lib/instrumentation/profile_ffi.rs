// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! FFI bindings for `LlvmProfdata` (`//src/lib/llvm-profdata`).

use core::mem::MaybeUninit;

use super::profile::{LiveData, LiveDataBounds, LlvmProfdata};

unsafe extern "C" {
    pub(super) fn cpp_llvm_profdata_new(out: &mut MaybeUninit<LlvmProfdata>);
    pub(super) fn cpp_llvm_profdata_init(
        profdata: &mut LlvmProfdata,
        build_id: *const u8,
        build_id_len: usize,
    );
    pub(super) fn cpp_llvm_profdata_size_bytes(profdata: &LlvmProfdata) -> usize;
    pub(super) fn cpp_llvm_profdata_write_fixed_data(
        profdata: &mut LlvmProfdata,
        data: *mut u8,
        data_len: usize,
        out: &mut MaybeUninit<LiveData>,
    );
    pub(super) fn cpp_llvm_profdata_bounds(
        profdata: &LlvmProfdata,
        out: &mut MaybeUninit<LiveDataBounds>,
    );
    pub(super) fn cpp_llvm_profdata_live_data_for_bounds(
        profdata: &LlvmProfdata,
        bounds_data: *mut u8,
        bounds_size: usize,
        out: &mut MaybeUninit<LiveData>,
    );
    pub(super) fn cpp_llvm_profdata_copy_live_data(profdata: &mut LlvmProfdata, data: &LiveData);
    pub(super) fn cpp_llvm_profdata_use_live_data(data: &LiveData);
}
