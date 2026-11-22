// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "phys/runtime/self.h"

#include <lib/elfldltl/self.h>

#include <fbl/no_destructor.h>
#include <ktl/byte.h>
#include <phys/elf/image.h>
#include <phys/runtime/symbolize.h>

#include <ktl/enforce.h>

// These are defined by the phys.ld linker script.
extern "C" ktl::byte __executable_start[], _edata[];
extern "C" const ktl::byte __start_note_gnu_build_id[];
extern "C" const ktl::byte __stop_note_gnu_build_id[];

namespace {

// This is function-local so it can be non-constinit.  It's never destroyed.
ElfImage& MutableSelf() {
  static fbl::NoDestructor<ElfImage> gSelf;
  return *gSelf;
}

}  // namespace

void InitSelf(const char* name) {
  using Phdr = ElfImage::Elf::Phdr;

  auto memory = elfldltl::Self<>::Memory(__executable_start, _end);
  auto bias = elfldltl::Self<>::LoadBias();

  Phdr load_segment = {
      .type = elfldltl::ElfPhdrType::kLoad,
      .vaddr = memory.base(),
      .filesz = _edata - __executable_start,
      .memsz = memory.image().size_bytes(),
  };
  load_segment.flags = Phdr::kRead | Phdr::kWrite | Phdr::kExecute;

  MutableSelf().InitSelf(name, memory, bias, load_segment,
                         {__start_note_gnu_build_id, __stop_note_gnu_build_id});
}

const ElfImage& Self() { return MutableSelf(); }

void PruneFromSelf(uint64_t start, uint64_t end) {
  MutableSelf().Prune(start, end);
  gSymbolize->ContextAlways();
}
