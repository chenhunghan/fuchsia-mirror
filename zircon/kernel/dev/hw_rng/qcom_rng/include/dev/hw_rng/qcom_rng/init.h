// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_HW_RNG_QCOM_RNG_INCLUDE_DEV_HW_RNG_QCOM_RNG_INIT_H_
#define ZIRCON_KERNEL_DEV_HW_RNG_QCOM_RNG_INCLUDE_DEV_HW_RNG_QCOM_RNG_INIT_H_

#include <lib/zbi-format/driver-config.h>
#include <zircon/compiler.h>

__BEGIN_CDECLS

// Initializes the driver.
void QcomRngInit(const zbi_dcfg_qcom_rng_t& config);

__END_CDECLS

#endif  // ZIRCON_KERNEL_DEV_HW_RNG_QCOM_RNG_INCLUDE_DEV_HW_RNG_QCOM_RNG_INIT_H_
