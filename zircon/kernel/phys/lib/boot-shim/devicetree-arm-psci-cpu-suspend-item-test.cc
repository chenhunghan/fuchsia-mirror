// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-shim/devicetree-boot-shim.h>
#include <lib/boot-shim/devicetree.h>
#include <lib/boot-shim/testing/devicetree-test-fixture.h>
#include <lib/fit/defer.h>
#include <lib/zbitl/image.h>

#include <algorithm>
#include <cinttypes>
#include <new>
#include <optional>
#include <vector>

namespace {

using boot_shim::testing::LoadDtb;
using boot_shim::testing::LoadedDtb;

template <uint64_t Value>
struct MockArmMpidr {
  static arch::ArmMultiprocessorAffinityRegister Read() {
    arch::ArmMultiprocessorAffinityRegister mpidr;
    mpidr.set_reg_value(Value);
    return mpidr;
  }
};

template <uint64_t BootMpidr>
using ArmDevicetreeCpuTopologyItem =
    boot_shim::ArmDevicetreeCpuTopologyItem<MockArmMpidr<BootMpidr>>;

class TestAllocator {
 public:
  TestAllocator() = default;
  TestAllocator(TestAllocator&& other) noexcept : allocs_(std::move(other.allocs_)) {
    other.allocs_.clear();
  }

  ~TestAllocator() {
    for (const Allocation& alloc : allocs_) {
      // Pairs with `new (align, &ac) uint8_t[size]`; required by ASan for alignments >= 8.
      ::operator delete[](alloc.ptr, alloc.alignment);
    }
  }

  void* operator()(size_t size, size_t alignment, fbl::AllocChecker& ac) {
    const auto align = static_cast<std::align_val_t>(alignment);
    uint8_t* alloc = new (align, &ac) uint8_t[size];
    if (alloc != nullptr) {
      allocs_.push_back({.ptr = alloc, .alignment = align});
    }
    return alloc;
  }

 private:
  struct Allocation {
    uint8_t* ptr = nullptr;
    std::align_val_t alignment{};
  };

  std::vector<Allocation> allocs_;
};

class ArmDevicetreePsciCpuSuspendItemTest
    : public boot_shim::testing::TestMixin<boot_shim::testing::ArmDevicetreeTest,
                                           boot_shim::testing::SyntheticDevicetreeTest> {
 public:
  static void SetUpTestSuite() {
    Mixin::SetUpTestSuite();
    auto loaded_dtb = LoadDtb("arm_idle_states.dtb");
    ASSERT_TRUE(loaded_dtb.is_ok(), "%s", loaded_dtb.error_value().c_str());
    idle_states_ = std::move(loaded_dtb).value();

    loaded_dtb = LoadDtb("arm_idle_states_and_domain.dtb");
    ASSERT_TRUE(loaded_dtb.is_ok(), "%s", loaded_dtb.error_value().c_str());
    idle_states_and_domain_ = std::move(loaded_dtb).value();

    loaded_dtb = LoadDtb("arm_idle_states_multiple.dtb");
    ASSERT_TRUE(loaded_dtb.is_ok(), "%s", loaded_dtb.error_value().c_str());
    idle_states_multiple_ = std::move(loaded_dtb).value();

    loaded_dtb = LoadDtb("arm_idle_states_multiple_with_invalid.dtb");
    ASSERT_TRUE(loaded_dtb.is_ok(), "%s", loaded_dtb.error_value().c_str());
    idle_states_multiple_with_invalid_ = std::move(loaded_dtb).value();

    loaded_dtb = LoadDtb("arm_idle_states_power_domains.dtb");
    ASSERT_TRUE(loaded_dtb.is_ok(), "%s", loaded_dtb.error_value().c_str());
    idle_states_power_domains_ = std::move(loaded_dtb).value();
  }

  static void TearDownTestSuite() {
    idle_states_ = std::nullopt;
    idle_states_and_domain_ = std::nullopt;
    idle_states_multiple_ = std::nullopt;
    idle_states_multiple_with_invalid_ = std::nullopt;
    idle_states_power_domains_ = std::nullopt;
    Mixin::TearDownTestSuite();
  }

  devicetree::Devicetree idle_states() { return idle_states_->fdt(); }
  devicetree::Devicetree idle_states_and_domain() { return idle_states_and_domain_->fdt(); }
  devicetree::Devicetree idle_states_multiple() { return idle_states_multiple_->fdt(); }
  devicetree::Devicetree idle_states_multiple_with_invalid() {
    return idle_states_multiple_with_invalid_->fdt();
  }
  devicetree::Devicetree idle_states_power_domains() { return idle_states_power_domains_->fdt(); }

 private:
  static inline std::optional<LoadedDtb> idle_states_;
  static inline std::optional<LoadedDtb> idle_states_and_domain_;
  static inline std::optional<LoadedDtb> idle_states_multiple_;
  static inline std::optional<LoadedDtb> idle_states_multiple_with_invalid_;
  static inline std::optional<LoadedDtb> idle_states_power_domains_;
};

TEST_F(ArmDevicetreePsciCpuSuspendItemTest, MissingNode) {
  alignas(ZBI_ALIGNMENT) std::array<std::byte, 1024> image_buffer{};
  std::vector<void*> allocs;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  auto fdt = empty_fdt();
  boot_shim::DevicetreeBootShim<boot_shim::ArmDevicetreePsciCpuSuspendItem> shim("test", fdt);
  shim.set_allocator(TestAllocator());
  ASSERT_TRUE(shim.Init());

  auto clear_errors = fit::defer([&]() { image.ignore_error(); });
  ASSERT_TRUE(shim.AppendItems(image).is_ok());
  for (auto [header, payload] : image) {
    EXPECT_FALSE(header->type == ZBI_TYPE_KERNEL_DRIVER &&
                 header->extra == ZBI_KERNEL_DRIVER_ARM_PSCI_CPU_SUSPEND);
  }
}

struct ExpectedIdleState {
  std::optional<uint32_t> id;
  uint32_t psci_param;
  uint32_t entry;
  uint32_t exit;
  uint32_t residency;
  bool is_domain_idle_state;
  bool local_timer_stops;
};

void CheckIdleStates(zbitl::Image<std::span<std::byte>>& zbi,
                     std::span<const ExpectedIdleState> states) {
  bool present = false;

  auto clear_errors = fit::defer([&]() { zbi.ignore_error(); });
  for (const auto& [header, payload] : zbi) {
    if (header->type != ZBI_TYPE_KERNEL_DRIVER) {
      continue;
    }
    if (header->extra != ZBI_KERNEL_DRIVER_ARM_PSCI_CPU_SUSPEND) {
      continue;
    }
    present = true;
    ASSERT_EQ(header->length, sizeof(zbi_dcfg_arm_psci_cpu_suspend_state_t) * states.size());
    auto* psci_states = reinterpret_cast<zbi_dcfg_arm_psci_cpu_suspend_state_t*>(payload.data());
    // Simple lookup based on the psci parameter.
    for (const auto& state : states) {
      bool state_found = false;
      for (const auto& psci_state : std::span(psci_states, states.size())) {
        if (psci_state.power_state != state.psci_param) {
          continue;
        }
        state_found = true;
        if (state.id.has_value()) {
          EXPECT_EQ(*state.id, psci_state.id);
        }
        EXPECT_EQ(
            (psci_state.flags & ZBI_ARM_PSCI_CPU_SUSPEND_STATE_FLAGS_TARGETS_POWER_DOMAIN) != 0,
            state.is_domain_idle_state);
        EXPECT_EQ((psci_state.flags & ZBI_ARM_PSCI_CPU_SUSPEND_STATE_FLAGS_LOCAL_TIMER_STOPS) != 0,
                  state.local_timer_stops);
        EXPECT_EQ(psci_state.entry_latency_us, state.entry);
        EXPECT_EQ(psci_state.exit_latency_us, state.exit);
        EXPECT_EQ(psci_state.min_residency_us, state.residency);
      }
      ASSERT_TRUE(state_found);
    }
  }
  ASSERT_TRUE(present);
}

TEST_F(ArmDevicetreePsciCpuSuspendItemTest, IdleStates) {
  alignas(ZBI_ALIGNMENT) std::array<std::byte, 1024> image_buffer{};
  std::vector<void*> allocs;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  auto fdt = idle_states();
  boot_shim::DevicetreeBootShim<boot_shim::ArmDevicetreePsciCpuSuspendItem> shim("test", fdt);
  shim.set_allocator(TestAllocator());

  ASSERT_TRUE(shim.Init());
  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  constexpr auto kExpected = std::to_array<ExpectedIdleState>({
      {
          .psci_param = 0x40000003,
          .entry = 0x123,
          .exit = 0x456,
          .residency = 0x589,
          .is_domain_idle_state = false,
          .local_timer_stops = true,
      },
      {
          .psci_param = 0x41000043,
          .entry = 0x321,
          .exit = 0x456,
          .residency = 0x987,
          .is_domain_idle_state = true,
          .local_timer_stops = false,
      },
  });

  CheckIdleStates(image, kExpected);
}

TEST_F(ArmDevicetreePsciCpuSuspendItemTest, IdleStatesAndDomainStates) {
  alignas(ZBI_ALIGNMENT) std::array<std::byte, 1024> image_buffer{};
  std::vector<void*> allocs;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  auto fdt = idle_states_and_domain();
  boot_shim::DevicetreeBootShim<boot_shim::ArmDevicetreePsciCpuSuspendItem> shim("test", fdt);
  shim.set_allocator(TestAllocator());

  ASSERT_TRUE(shim.Init());
  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  constexpr auto kExpected = std::to_array<ExpectedIdleState>({
      {
          .psci_param = 0x40000003,
          .entry = 0x123,
          .exit = 0x456,
          .residency = 0x589,
          .is_domain_idle_state = false,
          .local_timer_stops = true,
      },
      {
          .psci_param = 0x41000043,
          .entry = 0x321,
          .exit = 0x456,
          .residency = 0x987,
          .is_domain_idle_state = true,
          .local_timer_stops = false,
      },
  });

  CheckIdleStates(image, kExpected);
}

TEST_F(ArmDevicetreePsciCpuSuspendItemTest, IdleStatesMultiple) {
  alignas(ZBI_ALIGNMENT) std::array<std::byte, 1024> image_buffer{};
  std::vector<void*> allocs;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  auto fdt = idle_states_multiple();
  boot_shim::DevicetreeBootShim<boot_shim::ArmDevicetreePsciCpuSuspendItem> shim("test", fdt);
  shim.set_allocator(TestAllocator());

  ASSERT_TRUE(shim.Init());
  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  constexpr auto kExpected = std::to_array<ExpectedIdleState>({
      {
          .psci_param = 0x40000003,
          .entry = 0x123,
          .exit = 0x456,
          .residency = 0x589,
          .is_domain_idle_state = false,
          .local_timer_stops = true,
      },
      {
          .psci_param = 0x40000004,
          .entry = 0x124,
          .exit = 0x457,
          .residency = 0x580,
          .is_domain_idle_state = false,
          .local_timer_stops = false,
      },
      {
          .psci_param = 0x40000005,
          .entry = 0x122,
          .exit = 0x455,
          .residency = 0x588,
          .is_domain_idle_state = false,
          .local_timer_stops = true,
      },
      {
          .psci_param = 0x41000043,
          .entry = 0x321,
          .exit = 0x456,
          .residency = 0x987,
          .is_domain_idle_state = true,
          .local_timer_stops = false,
      },
  });

  CheckIdleStates(image, kExpected);
}

TEST_F(ArmDevicetreePsciCpuSuspendItemTest, IdleStatesMultipleWithInvalid) {
  alignas(ZBI_ALIGNMENT) std::array<std::byte, 1024> image_buffer{};
  std::vector<void*> allocs;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  auto fdt = idle_states_multiple_with_invalid();
  boot_shim::DevicetreeBootShim<boot_shim::ArmDevicetreePsciCpuSuspendItem> shim("test", fdt);
  shim.set_allocator(TestAllocator());

  ASSERT_TRUE(shim.Init());
  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  constexpr auto kExpected = std::to_array<ExpectedIdleState>({
      {
          .psci_param = 0x40000003,
          .entry = 0x123,
          .exit = 0x456,
          .residency = 0x589,
          .is_domain_idle_state = false,
          .local_timer_stops = true,
      },
      {
          .psci_param = 0x41000043,
          .entry = 0x321,
          .exit = 0x456,
          .residency = 0x987,
          .is_domain_idle_state = true,
          .local_timer_stops = false,
      },
  });

  CheckIdleStates(image, kExpected);
}

// Returns the ZBI_KERNEL_DRIVER_ARM_PSCI_CPU_SUSPEND entries in `zbi`.
std::vector<zbi_dcfg_arm_psci_cpu_suspend_state_t> FindSuspendStates(
    zbitl::Image<std::span<std::byte>>& zbi) {
  std::vector<zbi_dcfg_arm_psci_cpu_suspend_state_t> states;
  auto clear_errors = fit::defer([&] { zbi.ignore_error(); });
  for (const auto& [header, payload] : zbi) {
    if (header->type != ZBI_TYPE_KERNEL_DRIVER ||
        header->extra != ZBI_KERNEL_DRIVER_ARM_PSCI_CPU_SUSPEND) {
      continue;
    }
    EXPECT_TRUE(states.empty(), "more than one ZBI_KERNEL_DRIVER_ARM_PSCI_CPU_SUSPEND");
    EXPECT_EQ(0u, header->length % sizeof(zbi_dcfg_arm_psci_cpu_suspend_state_t));
    auto* entries = reinterpret_cast<const zbi_dcfg_arm_psci_cpu_suspend_state_t*>(payload.data());
    std::span span(entries, payload.size() / sizeof(zbi_dcfg_arm_psci_cpu_suspend_state_t));
    states.assign(span.begin(), span.end());
  }
  return states;
}

// Returns the bitmask of the logical IDs that the ZBI_TYPE_CPU_TOPOLOGY item in `zbi` assigns to
// the CPUs with `mpidrs`.
uint64_t LogicalCpus(zbitl::Image<std::span<std::byte>>& zbi, std::span<const uint64_t> mpidrs) {
  uint64_t cpus = 0;
  auto clear_errors = fit::defer([&] { zbi.ignore_error(); });
  for (const auto& [header, payload] : zbi) {
    if (header->type != ZBI_TYPE_CPU_TOPOLOGY) {
      continue;
    }
    std::span nodes(reinterpret_cast<const zbi_topology_node_t*>(payload.data()),
                    payload.size() / sizeof(zbi_topology_node_t));
    for (const zbi_topology_node_t& node : nodes) {
      if (node.entity.discriminant != ZBI_TOPOLOGY_ENTITY_PROCESSOR) {
        continue;
      }
      const zbi_topology_arm64_info_t& arm64 = node.entity.processor.architecture_info.arm64;
      const uint64_t mpidr = (uint64_t{arm64.cluster_3_id} << 32u) |
                             (uint64_t{arm64.cluster_2_id} << 16u) |
                             (uint64_t{arm64.cluster_1_id} << 8u) | uint64_t{arm64.cpu_id};
      if (std::ranges::find(mpidrs, mpidr) != mpidrs.end()) {
        cpus |= uint64_t{1} << node.entity.processor.logical_ids[0];
      }
    }
  }
  return cpus;
}

TEST_F(ArmDevicetreePsciCpuSuspendItemTest, AllCpusWithoutCpuNodes) {
  alignas(ZBI_ALIGNMENT) std::array<std::byte, 1024> image_buffer;
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  auto fdt = idle_states();
  boot_shim::DevicetreeBootShim<boot_shim::ArmDevicetreePsciCpuSuspendItem> shim("test", fdt);
  shim.set_allocator(TestAllocator());

  ASSERT_TRUE(shim.Init());
  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  std::vector states = FindSuspendStates(image);
  ASSERT_EQ(2u, states.size());
  for (const zbi_dcfg_arm_psci_cpu_suspend_state_t& state : states) {
    EXPECT_EQ(~uint64_t{0}, state.cpu_mask, "power_state 0x%" PRIx32, state.power_state);
  }
}

TEST_F(ArmDevicetreePsciCpuSuspendItemTest, AllCpusWithoutCpuTopology) {
  alignas(ZBI_ALIGNMENT) std::array<std::byte, 1024> image_buffer{};
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  auto fdt = idle_states_power_domains();
  boot_shim::DevicetreeBootShim<boot_shim::ArmDevicetreePsciCpuSuspendItem> shim("test", fdt);
  shim.set_allocator(TestAllocator());

  ASSERT_TRUE(shim.Init());
  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  // Without a CPU topology, no CPU can be mapped to a logical ID, so each enabled idle state is
  // emitted once for every CPU.
  std::vector states = FindSuspendStates(image);
  constexpr auto kExpectedPowerStates =
      std::to_array<uint32_t>({0x40000003, 0x40000004, 0x40010033, 0x40020333});
  ASSERT_EQ(kExpectedPowerStates.size(), states.size());
  for (size_t i = 0; i < states.size(); ++i) {
    EXPECT_EQ(i, states[i].id);
    EXPECT_EQ(kExpectedPowerStates[i], states[i].power_state);
    EXPECT_EQ(~uint64_t{0}, states[i].cpu_mask);
  }
}

template <uint64_t BootMpidr>
void CheckPowerDomainHierarchy(devicetree::Devicetree fdt) {
  alignas(ZBI_ALIGNMENT) std::array<std::byte, 4096> image_buffer{};
  zbitl::Image<std::span<std::byte>> image(image_buffer);
  ASSERT_TRUE(image.clear().is_ok());

  boot_shim::DevicetreeBootShim<ArmDevicetreeCpuTopologyItem<BootMpidr>,
                                boot_shim::ArmDevicetreePsciCpuSuspendItem>
      shim("test", fdt);
  shim.set_allocator(TestAllocator());

  ASSERT_TRUE(shim.Init());
  ASSERT_TRUE(shim.AppendItems(image).is_ok());

  // `cpu@400` (multi-entry `reg`) and `cpu` (missing `reg`) are not in the CPU topology.
  // `cpu@300`'s `reg` (<0x80000300>) has a non-affinity bit that is masked out to match its
  // topology entry, and its self-referential `power-domains` cycle terminates at the maximum depth.
  auto cpus = [&image](std::initializer_list<uint64_t> mpidrs) {
    return LogicalCpus(image, std::span(mpidrs.begin(), mpidrs.size()));
  };
  struct Expected {
    uint32_t power_state;
    bool targets_power_domain;
    uint64_t cpu_mask;
  };
  // CPU idle states are emitted once with every CPU that may enter them. Domain idle states are
  // emitted once per power domain that lists them, with the CPUs beneath that domain. The disabled
  // `disabled-c5` state is skipped.
  const auto kExpected = std::to_array<Expected>({
      // cpu-a-c2: per-CPU domains of cpu@0 and cpu@1, cpu@200 directly, and cpu@201 through
      // PD_CPU0.
      {0x40000003, false, cpus({0x0, 0x1, 0x200, 0x201})},
      // cpu-b-c2: per-CPU domains of cpu@100 and cpu@101, and cpu@201 directly.
      {0x40000004, false, cpus({0x100, 0x101, 0x201})},
      // cluster-b-c3 in PD_CLUSTER_B.
      {0x40010033, true, cpus({0x100, 0x101})},
      // cluster-b-c3 in PD_CYCLE.
      {0x40010033, true, cpus({0x300})},
      // top-c4 in PD_TOP.
      {0x40020333, true, cpus({0x0, 0x1, 0x100, 0x101, 0x201})},
  });

  std::vector states = FindSuspendStates(image);
  ASSERT_EQ(kExpected.size(), states.size());
  for (size_t i = 0; i < states.size(); ++i) {
    const zbi_dcfg_arm_psci_cpu_suspend_state_t& state = states[i];
    EXPECT_EQ(i, state.id);
    EXPECT_EQ(kExpected[i].power_state, state.power_state, "entry %zu", i);
    EXPECT_EQ(kExpected[i].targets_power_domain,
              (state.flags & ZBI_ARM_PSCI_CPU_SUSPEND_STATE_FLAGS_TARGETS_POWER_DOMAIN) != 0,
              "entry %zu", i);
    EXPECT_NE(0u, kExpected[i].cpu_mask, "entry %zu", i);
    EXPECT_EQ(kExpected[i].cpu_mask, state.cpu_mask, "entry %zu", i);
  }
}

TEST_F(ArmDevicetreePsciCpuSuspendItemTest, PowerDomainHierarchy) {
  ASSERT_NO_FATAL_FAILURE(CheckPowerDomainHierarchy<0x0>(idle_states_power_domains()));
}

// The boot CPU gets logical ID 0, so the masks must follow the topology's logical IDs rather than
// devicetree order.
TEST_F(ArmDevicetreePsciCpuSuspendItemTest, PowerDomainHierarchyNonZeroBootCpu) {
  ASSERT_NO_FATAL_FAILURE(CheckPowerDomainHierarchy<0x101>(idle_states_power_domains()));
}

}  // namespace
