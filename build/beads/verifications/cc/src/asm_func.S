// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

.text
.globl asm_example_identity
#if defined(__x86_64__)
asm_example_identity:
    mov %edi, %eax
    ret
#elif defined(__aarch64__)
asm_example_identity:
    ret
#else
asm_example_identity:
    ret
#endif
