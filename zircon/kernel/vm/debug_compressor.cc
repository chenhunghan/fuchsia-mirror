// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <vm/debug_compressor.h>
#include <vm/vm_constants.h>

static_assert(sizeof(VmDebugCompressor) == kVmDebugCompressorStorageSize);
static_assert(alignof(VmDebugCompressor) == kVmDebugCompressorStorageAlign);

extern "C" {
void rust_debug_compressor_init(void* storage);
void rust_debug_compressor_destroy(void* storage);
zx_status_t rust_debug_compressor_start(const void* compressor);
void rust_debug_compressor_add(const void* compressor, vm_page_t* page, VmCowPages* object,
                               uint64_t offset);
void rust_debug_compressor_pause(const void* compressor);
void rust_debug_compressor_resume(const void* compressor);
}  // extern "C"

VmDebugCompressor::VmDebugCompressor() { rust_debug_compressor_init(&storage_); }

VmDebugCompressor::~VmDebugCompressor() { rust_debug_compressor_destroy(&storage_); }

zx_status_t VmDebugCompressor::Init() { return rust_debug_compressor_start(&storage_); }

void VmDebugCompressor::Add(vm_page_t* page, VmCowPages* object, uint64_t offset) {
  rust_debug_compressor_add(&storage_, page, object, offset);
}

void VmDebugCompressor::Pause() { rust_debug_compressor_pause(&storage_); }

void VmDebugCompressor::Resume() { rust_debug_compressor_resume(&storage_); }
