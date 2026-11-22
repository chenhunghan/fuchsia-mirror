// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_TEST_CODE_PATCHING_TEST_H_
#define ZIRCON_KERNEL_PHYS_TEST_CODE_PATCHING_TEST_H_

// This file will be included into test.cc, add-one.S, and the generated
// `mutiply_by_factor` assembly stub.

// Concocted patch case IDs.
#define CASE_ID_ADD_ONE 17
#define CASE_ID_MULTIPLY_BY_FACTOR 35
#define CASE_ID_ADD_VALUE 53
#define CASE_ID_MULTIPLY_BY_FACTOR_TWIN 71

#if defined(__aarch64__)

#define PATCH_SIZE_ADD_ONE 4
#define PATCH_SIZE_MULTIPLY_BY_FACTOR 12

#elif defined(__x86_64__)

#define PATCH_SIZE_ADD_ONE 4
#define PATCH_SIZE_MULTIPLY_BY_FACTOR 8

#elif defined(__riscv)

#define PATCH_SIZE_ADD_ONE 2
#define PATCH_SIZE_MULTIPLY_BY_FACTOR 12

#else
#error "unknown architecture"
#endif

#ifndef __ASSEMBLER__

#include <cstddef>
#include <cstdint>

#include <ktl/byte.h>
#include <ktl/span.h>
#include <ktl/string_view.h>

constexpr uint32_t kAddOneCaseId = CASE_ID_ADD_ONE;
constexpr size_t kAddOnePatchSize = PATCH_SIZE_ADD_ONE;

constexpr uint32_t kMultiplyByFactorCaseId = CASE_ID_MULTIPLY_BY_FACTOR;
constexpr size_t kMultiplyByFactorPatchSize = PATCH_SIZE_MULTIPLY_BY_FACTOR;

constexpr uint32_t kAddValueCaseId = CASE_ID_ADD_VALUE;

constexpr uint32_t kMultiplyByFactorTwinCaseId = CASE_ID_MULTIPLY_BY_FACTOR_TWIN;

// Mark with cfi_unchecked_callee since these are defined in ASM so CFI will
// not generate appropriate metadata for them.
using TestFn = uint64_t(uint64_t x) [[clang::cfi_unchecked_callee]];

// Defined in add-one.S.
extern "C" TestFn AddOne;

// Defined via code_patching_hermetic_stub().
extern "C" TestFn multiply_by_factor;
extern "C" TestFn multiply_by_factor_twin;

// Defined via code_patching_hermetic_multiversion().
extern "C" TestFn add_value;

#endif  // !__ASSEMBLER__

#endif  // ZIRCON_KERNEL_PHYS_TEST_CODE_PATCHING_TEST_H_
