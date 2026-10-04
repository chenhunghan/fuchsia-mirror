// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/elfldltl/init-fini.h>
#include <lib/ld/module.h>

#include <mutex>

#include "../dlfcn/dlfcn-abi.h"
#include "../startup/start-main.h"
#include "ld-abi.h"

namespace LIBC_NAMESPACE_DECL {

using InitFiniInfo = elfldltl::InitFiniInfo<elfldltl::Elf<>>;

void StartupCtors() {
  // Lock against dlopen in a ctor-spawned thread, so all these ctors always
  // run first before the new thread's new dlopen'd modules' ctors.
  std::lock_guard lock(kDlfcnLock);

  // TODO(https://fxbug.dev/338239201): Also, if one of the ctors run here
  // calls dlopen, that should make sure that all the other pending startup
  // module ctors have been completed in load order before the dlopen'd
  // modules' ctors.

  // Only the executable's DT_PREINIT_ARRAY is recorded in the passive ABI, so
  // it's outside the Module.  Run those first.  The load bias doesn't need to
  // be fetched because that's only used for the legacy pointer when the array
  // is relocated (as it is here), and preinit has no legacy pointer.
  auto preinit = InitFiniInfo{_ld_abi.preinit_array}.callable_init_no_legacy();
  preinit();

  // Run normal initializers for all the modules.
  auto init = ld::AbiCallableInit(_ld_abi);
  init();
}

}  // namespace LIBC_NAMESPACE_DECL
