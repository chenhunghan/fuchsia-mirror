// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_PLATFORM_EFI_H_
#define ZIRCON_KERNEL_INCLUDE_PLATFORM_EFI_H_

// Returns true if the EFI tests should be expected to succeed.
extern "C" bool IsEfiExpected();

#endif  // ZIRCON_KERNEL_INCLUDE_PLATFORM_EFI_H_
