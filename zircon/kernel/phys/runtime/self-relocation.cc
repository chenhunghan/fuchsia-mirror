// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/elfldltl/diagnostics.h>
#include <lib/elfldltl/static-pie.h>

#include <phys/runtime/main.h>
#include <phys/runtime/self.h>

// This is called from start.S just before PhysMain, once the stack and
// thread pointer are set up.  It is not declared in any header: nothing but
// the entry code should call it, and it must not be called twice.
extern "C" void ApplyRelocationsToSelf();

// Apply any relocations to our binary.
//
// Phys binaries are position-independent, so this is required to ensure
// pointers in data sections, vtables, etc. are updated to their correct
// locations when loaded somewhere other than the link-time address.
extern "C" void ApplyRelocationsToSelf() {
  // The phys program may be loaded at its link-time address, observed at
  // runtime as Self::LoadBias() == 0, in which case this does nothing.
  auto diag = elfldltl::TrapDiagnostics();
  elfldltl::LinkStaticPie(elfldltl::Self<>(), diag, PHYS_LOAD_ADDRESS, _end);
}
