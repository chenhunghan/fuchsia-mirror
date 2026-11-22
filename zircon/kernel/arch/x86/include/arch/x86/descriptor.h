// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2009 Corey Tabaka
// Copyright (c) 2014 Intel Corporation
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_DESCRIPTOR_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_DESCRIPTOR_H_

/*
 * System Selectors
 */
#define NULL_SELECTOR 0x00

/********* kernel selectors *********/
#define CODE_SELECTOR 0x08
#define CODE_64_SELECTOR 0x10
#define DATA_SELECTOR 0x18

/********* user selectors *********/
#define USER_CODE_SELECTOR (0x20 | 3)
#define USER_DATA_SELECTOR (0x28 | 3)
#define USER_CODE_64_SELECTOR (0x30 | 3)

#define TSS_SELECTOR(i) ((uint16_t)(0x38 + 16 * (i)))
/* 0x40 is used by the second half of the first TSS descriptor */

/* selector priviledge level */
#define SELECTOR_PL(s) ((s) & 0x3)

/*
 * Descriptor Types
 */
#define SEG_TYPE_TSS 0x9
#define SEG_TYPE_TSS_BUSY 0xb
#define SEG_TYPE_TASK_GATE 0x5
#define SEG_TYPE_INT_GATE 0xe /* 32 bit */
#define SEG_TYPE_DATA_RW 0x2
#define SEG_TYPE_CODE_RW 0xa

#ifndef __ASSEMBLER__

#include <zircon/compiler.h>

#include <arch/aspace.h>
#include <arch/x86.h>
#include <arch/x86/ioport.h>
#include <arch/x86/ioport_ffi.h>

inline void x86_set_tss_io_bitmap(const IoBitmap& bitmap) { rust_x86_set_tss_io_bitmap(bitmap); }
inline void x86_clear_tss_io_bitmap(const IoBitmap& bitmap) {
  rust_x86_clear_tss_io_bitmap(bitmap);
}

typedef uint16_t seg_sel_t;

/* tss stuff */
__BEGIN_CDECLS

void x86_clear_tss_busy(seg_sel_t sel);
uintptr_t gdt_get();

__END_CDECLS

#endif

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_DESCRIPTOR_H_
