// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_LOCKDEP_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_LOCKDEP_H_

#include <lockdep/guard.h>
#include <lockdep/guard_multiple.h>
#include <lockdep/lockdep.h>

// Bring some lockdep types into global namespace for the kernel.
// TODO(eieio): Is there a better namespace to put these in, or perhaps they
// should just be used from the lockdep namespace?
using lockdep::AdoptLock;
using lockdep::AliasedLock;
using lockdep::AssertHeld;
using lockdep::AssertOrderedLock;
using lockdep::Guard;
using lockdep::GuardMultiple;
using lockdep::Lock;
using lockdep::NullGuard;

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_LOCKDEP_H_
