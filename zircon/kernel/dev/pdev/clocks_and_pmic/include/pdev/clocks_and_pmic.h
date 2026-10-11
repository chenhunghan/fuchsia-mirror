// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_PDEV_CLOCKS_AND_PMIC_INCLUDE_PDEV_CLOCKS_AND_PMIC_H_
#define ZIRCON_KERNEL_DEV_PDEV_CLOCKS_AND_PMIC_INCLUDE_PDEV_CLOCKS_AND_PMIC_H_

#include <zircon/compiler.h>

#include <dev/clocks_and_pmic.h>

// clocks_and_pmic interface
struct pdev_clocks_and_pmic_ops {
  zx_status_t (*prepare_for_suspend)();
  zx_status_t (*wakeup_from_suspend)();
};

static_assert(sizeof(pdev_clocks_and_pmic_ops) == 16, "pdev_clocks_and_pmic_ops size mismatch");
static_assert(alignof(pdev_clocks_and_pmic_ops) == 8, "pdev_clocks_and_pmic_ops align mismatch");

void pdev_register_clocks_and_pmic(const pdev_clocks_and_pmic_ops* ops);

#endif  // ZIRCON_KERNEL_DEV_PDEV_CLOCKS_AND_PMIC_INCLUDE_PDEV_CLOCKS_AND_PMIC_H_
