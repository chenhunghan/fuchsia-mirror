// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <dev/hw_rng.h>

extern "C" {

size_t rust_hw_rng_get_entropy(void* buf, size_t len);
void rust_hw_rng_register(const struct hw_rng_ops* ops);
bool rust_hw_rng_is_registered();

}  // extern "C"

size_t hw_rng_get_entropy(void* buf, size_t len) { return rust_hw_rng_get_entropy(buf, len); }

void hw_rng_register(const struct hw_rng_ops* ops) { rust_hw_rng_register(ops); }

bool hw_rng_is_registered() { return rust_hw_rng_is_registered(); }
