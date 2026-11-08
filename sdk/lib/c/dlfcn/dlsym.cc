// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/dlfcn/dlsym.h"

#include <dlfcn.h>
#include <lib/elfldltl/resolve.h>
#include <lib/ld/load-module.h>
#include <lib/ld/module.h>
#include <lib/ld/tls.h>

#include <cassert>
#include <ranges>
#include <utility>

#include "../ld/ld-abi.h"
#include "src/__support/common.h"

namespace LIBC_NAMESPACE_DECL {
namespace {

// When libdl is not present and so there can be no valid module handle from a
// dlopen call to pass dlsym, it can still be used with the two special-case
// pseudo-handle values RTLD_DEFAULT and RTLD_NEXT.

using namespace std::literals;

using AbiModule = ld::abi::Abi<>::Module;
using Sym = elfldltl::Elf<>::Sym;
using size_type = elfldltl::Elf<>::size_type;

// Both RTLD_DEFAULT and RTLD_NEXT lookups resolve a symbol over an ordered
// range of passive ABI modules.  RTLD_NEXT uses a tail subrange.

auto AllModules() { return ld::AbiLoadedSymbolModules(_ld_abi); }

void* DlsymOn(std::ranges::forward_range auto&& abi_modules, elfldltl::SymbolName name) {
  // TODO(https://fxbug.dev/325494556): Replace these with dlerror plumbing.
  auto diag = elfldltl::Diagnostics{[](auto&&...) { return false; }};

  // Return wrapper for all failures.  Called with no arguments when diag
  // already has all the state to report.  Called with arguments to add an
  // error to the diag object first.
  auto failure = [&diag](auto&&... args) -> void* {
    if constexpr (sizeof...(args) > 0) {
      diag.FormatError(std::forward<decltype(args)>(args)...);
    }
    return nullptr;
  };

  // Return wrapper for success.
  auto success = [](void* ptr) -> void* {
    // TODO(https://fxbug.dev/325494556): Success return must clear prior
    // dlerror() state.
    return ptr;
  };

  auto result = elfldltl::ResolveSymbol(
      diag, abi_modules,
      [&name](const AbiModule& module) mutable -> fit::result<bool, const Sym*> {
        return fit::ok(name.Lookup(module.symbols));
      },
      ld::kResolverPolicy);
  if (result.is_error()) [[unlikely]] {
    return failure();
  }

  if (!result->symbol) {
    return failure("undefined symbol");
  }

  switch (result->symbol->type()) {
    [[unlikely]] case elfldltl::ElfSymType::kIfunc:
      return failure("STT_IFUNC symbol "sv, name, " not handled");

    case elfldltl::ElfSymType::kTls: {
      const size_type modid = result->module->tls_modid;
      if (modid == 0) [[unlikely]] {
        const char* module_name = result->module->link_map.name.get();
        return failure("STT_TLS symbol "sv, name, " in module with no PT_TLS: "sv, module_name);
      }
      void* ptr = ld::TlsInitialExecData(_ld_abi, modid, result->symbol->value);
      return success(ptr);
    }

    default:
      // Other types, including SHN_ABS, just have the bias applied.
      break;
  }

  const size_type load_bias = result->module->link_map.addr;
  const uintptr_t address = result->symbol->value + load_bias;
  return success(reinterpret_cast<void*>(address));
}

// RTLD_DEFAULT searches all the visible modules in order.
void* DlsymDefault(const char* _Nonnull name) { return DlsymOn(AllModules(), name); }

// RTLD_NEXT skips ahead in the RTLD_DEFAULT list, to just past the module
// containing the immediate caller of dlsym, identified by the return PC to it.
void* DlsymNext(uintptr_t caller_pc, const char* _Nonnull name) {
  auto modules = AllModules();
  // The caller PC is a return address, so just in case, look instead just
  // before that to find the module containing the call instruction.
  auto it = ld::FindModuleByVaddr(modules, caller_pc - 1);
  if (it == modules.end()) [[unlikely]] {
    // TODO(https://fxbug.dev/325494556): This should cause dlerror() to yield
    // "RTLD_NEXT used from unknown caller module".  If the caller PC was not
    // in a module in the global resolution list, this is an invalid call.  Any
    // call from code not managed by the dynamic linker at all is never valid.
    // In libdl's RTLD_NEXT called from a dlopen'd, it instead needs to figure
    // out what resolution list(s) the caller module is part of and use the
    // appropriate resolution list for its point of view; the exact expected
    // semantics in all corners still need to be teased out by testing glibc
    // for reference.
    return nullptr;
  }
  return DlsymOn(std::ranges::subrange{++it, modules.end()}, name);
}

}  // namespace

// Never inline this (via LTO), so __builtin_return_address(0) is well-defined.
[[gnu::noinline]]
LLVM_LIBC_FUNCTION(void*, dlsym, (void* __restrict handle, const char* __restrict name)) {
  // TODO(https://fxbug.dev/338229633): Caller API violation(?).  Double-check
  // that it's truly invalid to pass nullptr and what glibc does about it.
  // See if we can put the nonnull attribute into the public header.
  assert(name);

  // TODO(https://fxbug.dev/338229633): If libdl is present at all, always just
  // tail-call to it only after distinguishing the special-case handles.  This
  // means that libdl before any dlopen calls has to do exactly all the work
  // we'd do here.  But note that for dlsym we're obliged to keep it minimal so
  // it can happen before most constructors have run and such (perhaps
  // including libdl's own) and perhaps in e.g. interposers on system call
  // functions--so it really shouldn't do things like take locks and allocate
  // memory as libdl's full initialization presumably would.  So if it has a
  // no-state-yet path to avoid its usual dynamic data structures (that don't
  // add value until there's a dlopen), that would be pretty exactly
  // copying/sharing the code above here.  So worst case the source should be
  // shared in header inlines.  But perhaps instead the _dlfcn_dlsym_* calls
  // should have a way to return "not it" non-failure so we punt back to the
  // no-libdl behavior here directly.

  if (handle == RTLD_DEFAULT) {
    // TODO(https://fxbug.dev/338229633): Weak<_dlfcn_dlsym_default>::Fallback
    return DlsymDefault(name);
  }

  if (handle == RTLD_NEXT) {
    const uintptr_t caller_pc =  // The immediate caller must be captured here.
        reinterpret_cast<uintptr_t>(__builtin_return_address(0));
    // TODO(https://fxbug.dev/338229633): Weak<_dlfcn_dlsym_next>::Fallback
    return DlsymNext(caller_pc, name);
  }

  // TODO(https://fxbug.dev/325494556): This should cause dlerror() to yield
  // "invalid handle, dlopen not linked in"(ish), something like that.
  return nullptr;
}

}  // namespace LIBC_NAMESPACE_DECL
