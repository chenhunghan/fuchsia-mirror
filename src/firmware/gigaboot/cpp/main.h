// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef SRC_FIRMWARE_GIGABOOT_CPP_MAIN_H_
#define SRC_FIRMWARE_GIGABOOT_CPP_MAIN_H_

#include <efi/protocol/loaded-image.h>
#include <efi/system-table.h>
#include <efi/types.h>

// This is the entry point in the PE-COFF headers.
extern "C" efi_status EfiMain(efi_handle image, efi_system_table* systab);

// Sets *stdout to write to the console output if there is one, else to the
// serial I/O protocol if there is one.
void SetEfiStdout(efi_system_table* systab);

// These are set by EfiMain.
extern efi_handle gEfiImageHandle;
extern efi_loaded_image_protocol* gEfiLoadedImage;
extern efi_system_table* gEfiSystemTable;

#endif  // SRC_FIRMWARE_GIGABOOT_CPP_MAIN_H_
