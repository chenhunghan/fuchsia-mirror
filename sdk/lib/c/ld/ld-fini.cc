// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/ld/module.h>

#include <algorithm>

#include "ld-abi.h"
#include "libc.h"
#include "src/__support/common.h"

namespace LIBC_NAMESPACE_DECL {
namespace {

void ModulesFini() {
  // TODO(https://fxbug.dev/338239201): Mitigate dlopen/dlclose calls either
  // racing in other threads or directly in fini functions.

  auto fini = ld::AbiCallableFini(_ld_abi);
  fini();
}

}  // namespace
}  // namespace LIBC_NAMESPACE_DECL

// This is the `extern "C"` name also defined by musl's integrated dynamic
// linker.  A name in LIBC_NAMESPACE will be used directly when that's gone.
void __libc_exit_fini() { LIBC_NAMESPACE::ModulesFini(); }
