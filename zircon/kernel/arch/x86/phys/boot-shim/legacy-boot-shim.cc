// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "legacy-boot-shim.h"

#include <lib/boot-shim/boot-shim.h>
#include <lib/fit/defer.h>
#include <lib/memalloc/pool.h>
#include <lib/uart/all.h>
#include <lib/zbi-format/zbi.h>
#include <lib/zbitl/items/kernel.h>
#include <stdlib.h>

#include <ktl/optional.h>
#include <ktl/span.h>
#include <phys/address-space.h>
#include <phys/allocation.h>
#include <phys/boot-zbi/boot-zbi.h>
#include <phys/runtime/main.h>
#include <phys/symbolize.h>
#include <phys/uart/driver.h>

#include "stdout.h"

#include <ktl/enforce.h>

namespace {

ktl::optional<uart::all::Config<>> GetUartFromRange(  //
    LegacyBootShim::InputZbi::iterator start, LegacyBootShim::InputZbi::iterator end) {
  ktl::optional<uart::all::Config<>> uart_config;

  while (start != end && start != start.view().end()) {
    auto& [header, payload] = *start;
    if (header->type == ZBI_TYPE_KERNEL_DRIVER) {
      if (ktl::optional config = uart::all::Config<>::Match(*header, payload.data())) {
        uart_config = *config;
      }
    }
    start++;
  }
  return uart_config;
}

uart::all::Config<> UartFromZbi(LegacyBootShim::InputZbi zbi,
                                const uart::all::Config<>& uart_config) {
  auto cleanup = fit::defer([&zbi]() { zbi.ignore_error(); });

  if (ktl::optional new_uart_config = GetUartFromRange(zbi.begin(), zbi.end())) {
    return *new_uart_config;
  }
  return uart_config;
}

}  // namespace

void PhysMain(void* ptr, arch::EarlyTicks boot_ticks) {
  ApplyRelocations();

  Symbolize symbolize(kLegacyShimName);

  // This also fills in gLegacyBoot.
  AddressSpace aspace;
  InitMemory(ptr, {}, &aspace);

  const ktl::span ramdisk = ktl::as_bytes(gLegacyBoot.ramdisk);
  gLegacyBoot.uart_config = UartFromZbi(LegacyBootShim::InputZbi(ramdisk), gLegacyBoot.uart_config);
  UartFromCmdLine(gLegacyBoot.cmdline, gLegacyBoot.uart_config);
  SetUartConsole(gLegacyBoot.uart_config);

  LegacyBootShim shim(symbolize.name(), gLegacyBoot);
  shim.set_build_id(symbolize.build_id());
  shim.Get<boot_shim::UartItem<>>().Init(GetUartDriver().config());

  // The pool knows all the memory details, so populate the ZBI item that way.
  memalloc::Pool& memory = Allocation::GetPool();
  shim.InitMemConfig(memory);

  BootZbi boot;
  if (shim.Load(boot)) {
    memory.PrintMemoryRanges(symbolize.name());
    boot.Log();
    boot.Boot();
  }

  abort();
}

bool LegacyBootShim::Load(BootZbi& boot) {
  return Check("Not a bootable ZBI", boot.Init(input_zbi())) &&
         Check("Failed to load ZBI", boot.Load(static_cast<uint32_t>(size_bytes()))) &&
         Check("Failed to append boot loader items to data ZBI", AppendItems(boot.DataZbi()));
}
