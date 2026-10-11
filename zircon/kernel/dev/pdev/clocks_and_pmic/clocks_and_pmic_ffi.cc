// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
//
// Ported from zircon/kernel/dev/pdev/clocks_and_pmic/clocks_and_pmic.cc

#include <zircon/types.h>

#include <dev/clocks_and_pmic.h>
#include <pdev/clocks_and_pmic.h>

extern "C" {

void rust_pdev_register_clocks_and_pmic(const pdev_clocks_and_pmic_ops* ops);
zx_status_t rust_clocks_and_pmic_prepare_for_suspend();
zx_status_t rust_clocks_and_pmic_wakeup_from_suspend();

}  // extern "C"

void pdev_register_clocks_and_pmic(const struct pdev_clocks_and_pmic_ops* ops) {
  rust_pdev_register_clocks_and_pmic(ops);
}

zx_status_t clocks_and_pmic_prepare_for_suspend() {
  return rust_clocks_and_pmic_prepare_for_suspend();
}

zx_status_t clocks_and_pmic_wakeup_from_suspend() {
  return rust_clocks_and_pmic_wakeup_from_suspend();
}
