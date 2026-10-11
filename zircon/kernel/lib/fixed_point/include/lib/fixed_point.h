// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2013, Google Inc. All rights reserved.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_FIXED_POINT_INCLUDE_LIB_FIXED_POINT_H_
#define ZIRCON_KERNEL_LIB_FIXED_POINT_INCLUDE_LIB_FIXED_POINT_H_

#include <stdint.h>

struct fp_32_64 {
  uint32_t l0;  /* unshifted value */
  uint32_t l32; /* value shifted left 32 bits (or bit -1 to -32) */
  uint32_t l64; /* value shifted left 64 bits (or bit -33 to -64) */
};

#ifdef __cplusplus
static_assert(sizeof(struct fp_32_64) == 12);
static_assert(alignof(struct fp_32_64) == 4);
#endif

#ifdef __cplusplus
extern "C" {
#endif

void rust_fixed_point_fp_32_64_div_32_32(struct fp_32_64* result, uint32_t dividend,
                                         uint32_t divisor);
uint64_t rust_fixed_point_u64_mul_u32_fp32_64(uint32_t a, struct fp_32_64 b);
uint32_t rust_fixed_point_u32_mul_u64_fp32_64(uint64_t a, struct fp_32_64 b);
uint64_t rust_fixed_point_u64_mul_u64_fp32_64(uint64_t a, struct fp_32_64 b);

#ifdef __cplusplus
}  // extern "C"
#endif

static inline void fp_32_64_div_32_32(struct fp_32_64* result, uint32_t dividend,
                                      uint32_t divisor) {
  rust_fixed_point_fp_32_64_div_32_32(result, dividend, divisor);
}

static inline uint64_t u64_mul_u32_fp32_64(uint32_t a, struct fp_32_64 b) {
  return rust_fixed_point_u64_mul_u32_fp32_64(a, b);
}

static inline uint32_t u32_mul_u64_fp32_64(uint64_t a, struct fp_32_64 b) {
  return rust_fixed_point_u32_mul_u64_fp32_64(a, b);
}

static inline uint64_t u64_mul_u64_fp32_64(uint64_t a, struct fp_32_64 b) {
  return rust_fixed_point_u64_mul_u64_fp32_64(a, b);
}

#endif  // ZIRCON_KERNEL_LIB_FIXED_POINT_INCLUDE_LIB_FIXED_POINT_H_
