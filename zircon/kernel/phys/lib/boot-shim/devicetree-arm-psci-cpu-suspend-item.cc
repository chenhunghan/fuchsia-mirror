// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/arm64/system.h>
#include <lib/boot-shim/devicetree.h>

#include <cinttypes>
#include <new>

namespace boot_shim {
namespace {

// See https://www.kernel.org/doc/Documentation/devicetree/bindings/arm/idle-states.txt
constexpr std::string_view kIdleStateCompatible = "arm,idle-state";

// See https://www.kernel.org/doc/Documentation/devicetree/bindings/power/domain-idle-state.yaml
constexpr std::string_view kDomainIdleStateCompatible = "domain-idle-state";

constexpr auto kIdleStateCompatibles =
    std::to_array({kIdleStateCompatible, kDomainIdleStateCompatible});

constexpr auto kDomainIdleStateIt = std::next(kIdleStateCompatibles.begin());

constexpr bool IsDomainIdleState(decltype(kIdleStateCompatibles)::const_iterator it) {
  return kDomainIdleStateIt == it;
}

constexpr bool IsPowerDomainState(const zbi_dcfg_arm_psci_cpu_suspend_state_t& state) {
  return (state.flags & ZBI_ARM_PSCI_CPU_SUSPEND_STATE_FLAGS_TARGETS_POWER_DOMAIN) != 0;
}

}  // namespace

devicetree::ScanState ArmDevicetreePsciCpuSuspendItem::OnIdleStateCount(
    const devicetree::PropertyDecoder& decoder) {
  auto compatibles =
      decoder.FindAndDecodeProperty<&devicetree::PropertyValue::AsStringList>("compatible");
  if (!compatibles) {
    OnError("Failed to parse 'compatible' property");
    return devicetree::ScanState::kActive;
  }

  if (std::ranges::find_first_of(kIdleStateCompatibles, *compatibles) ==
      kIdleStateCompatibles.end()) {
    return devicetree::ScanState::kActive;
  }
  num_idle_states_++;
  return devicetree::ScanState::kActive;
}

devicetree::ScanState ArmDevicetreePsciCpuSuspendItem::OnIdleStateFill(
    const devicetree::PropertyDecoder& decoder) {
  auto compatibles =
      decoder.FindAndDecodeProperty<&devicetree::PropertyValue::AsStringList>("compatible");
  if (!compatibles) {
    OnError("Failed to parse 'compatible' property");
    return devicetree::ScanState::kActive;
  }
  auto it = std::find_first_of(kIdleStateCompatibles.begin(), kIdleStateCompatibles.end(),
                               compatibles->begin(), compatibles->end());
  if (it == kIdleStateCompatibles.end()) {
    return devicetree::ScanState::kActive;
  }

  uint32_t flags =
      IsDomainIdleState(it) ? ZBI_ARM_PSCI_CPU_SUSPEND_STATE_FLAGS_TARGETS_POWER_DOMAIN : 0;
  auto [local_timer_stop, psci_param, entry, exit, residency] =
      decoder.FindProperties("local-timer-stop", "arm,psci-suspend-param", "entry-latency-us",
                             "exit-latency-us", "min-residency-us");

  if (local_timer_stop) {
    flags |= ZBI_ARM_PSCI_CPU_SUSPEND_STATE_FLAGS_LOCAL_TIMER_STOPS;
  }

  std::optional<uint32_t> parsed_psci_param;
  if (!psci_param) {
    Log("idle-state without `arm,psci-suspend-param` property.\n");
    return devicetree::ScanState::kActive;
  }

  parsed_psci_param = psci_param->AsUint32();
  if (!parsed_psci_param) {
    Log("Failed to parse idle-state `arm,psci-suspend-param` property.\n");
    return devicetree::ScanState::kActive;
  }

  auto parse_u32 = [this](auto maybe_property,
                          std::string_view prop_name) -> std::optional<uint32_t> {
    if (!maybe_property) {
      Log("`%.*s` property missing.\n", static_cast<int>(prop_name.size()), prop_name.data());
      return std::nullopt;
    }

    auto parsed = maybe_property->AsUint32();
    if (!parsed) {
      Log("Failed to parse `%.*s` property.\n", static_cast<int>(prop_name.size()),
          prop_name.data());
    }
    return parsed;
  };

  std::optional<uint32_t> entry_latency = parse_u32(entry, "entry-latency-us");
  std::optional<uint32_t> exit_latency = parse_u32(exit, "exit-latency-us");
  std::optional<uint32_t> min_residency = parse_u32(residency, "min-residency-us");

  if (current_idle_state_ >= num_idle_states_) {
    return devicetree::ScanState::kActive;
  }

  auto& idle_state = idle_states()[current_idle_state_];
  idle_state = {
      .id = current_idle_state_,
      .power_state = *parsed_psci_param,
      .flags = flags,
      .entry_latency_us = entry_latency.value_or(0),
      .exit_latency_us = exit_latency.value_or(0),
      .min_residency_us = min_residency.value_or(0),
      .cpu_mask = 0,
  };
  state_phandles_[current_idle_state_] =
      decoder.FindAndDecodeProperty<&devicetree::PropertyValue::AsUint32>("phandle").value_or(0);
  ++current_idle_state_;
  return devicetree::ScanState::kActive;
}

devicetree::ScanState ArmDevicetreePsciCpuSuspendItem::OnCpu(
    const devicetree::PropertyDecoder& decoder) {
  auto [device_type, reg_prop, idle_states, power_domains] =
      decoder.FindProperties("device_type", "reg", "cpu-idle-states", "power-domains");
  if (!device_type || device_type->AsString() != "cpu") {
    return devicetree::ScanState::kActive;
  }

  if (!allocated()) {
    num_cpus_++;
    return devicetree::ScanState::kActive;
  }

  if (current_cpu_ >= num_cpus_) {
    return devicetree::ScanState::kActive;
  }

  std::optional<devicetree::RegProperty> reg;
  if (reg_prop) {
    reg = reg_prop->AsReg(decoder);
  }
  std::optional<uint64_t> address;
  if (reg && reg->size() == 1) {
    address = (*reg)[0].address();
  }
  if (!address) {
    Log("CPU node with missing or malformed 'reg' property.\n");
    return devicetree::ScanState::kActive;
  }

  const uint64_t mpidr =
      arch::ArmMultiprocessorAffinityRegister{}.set_reg_value(*address).affinity();
  new (&cpus_[current_cpu_++]) CpuEntry{
      .mpidr = mpidr,
      .idle_states = idle_states ? idle_states->AsBytes() : devicetree::ByteView{},
      .power_domains = power_domains ? power_domains->AsBytes() : devicetree::ByteView{},
  };
  return devicetree::ScanState::kActive;
}

devicetree::ScanState ArmDevicetreePsciCpuSuspendItem::OnPowerDomain(
    const devicetree::PropertyDecoder& decoder) {
  auto [cells, phandle, idle_states, parents] = decoder.FindProperties(
      "#power-domain-cells", "phandle", "domain-idle-states", "power-domains");
  if (!cells || cells->AsUint32() != 0u || !phandle) {
    return devicetree::ScanState::kActive;
  }

  if (!allocated()) {
    num_power_domains_++;
    return devicetree::ScanState::kActive;
  }

  std::optional<uint32_t> parsed_phandle = phandle->AsUint32();
  if (current_power_domain_ >= num_power_domains_ || !parsed_phandle) {
    return devicetree::ScanState::kActive;
  }

  new (&power_domains_[current_power_domain_++]) PowerDomainEntry{
      .phandle = *parsed_phandle,
      .idle_states = idle_states ? idle_states->AsBytes() : devicetree::ByteView{},
      .parents = parents ? parents->AsBytes() : devicetree::ByteView{},
  };
  return devicetree::ScanState::kActive;
}

devicetree::ScanState ArmDevicetreePsciCpuSuspendItem::OnNode(
    const devicetree::NodePath& path, const devicetree::PropertyDecoder& decoder) {
  // Start counting nodes.
  if (path.back() == "idle-states") {
    idle_states_ = &path.back();
    return devicetree::ScanState::kActive;
  }

  if (path.back() == "domain-idle-states") {
    domain_idle_states_ = &path.back();
    return devicetree::ScanState::kActive;
  }

  if (idle_states_) {
    if (!allocated()) {
      return OnIdleStateCount(decoder);
    }
    return OnIdleStateFill(decoder);
  }

  // No point in specializing, since domain idle states may be contained
  // in `idle-states` anyway or on a separate node.
  if (domain_idle_states_) {
    if (!allocated()) {
      return OnIdleStateCount(decoder);
    }
    return OnIdleStateFill(decoder);
  }

  if (path.IsChildOf("/cpus") && path.back().name() == "cpu") {
    return OnCpu(decoder);
  }

  if (psci_) {
    return OnPowerDomain(decoder);
  }

  if (auto compatibles =
          decoder.FindAndDecodeProperty<&devicetree::PropertyValue::AsStringList>("compatible")) {
    for (std::string_view compatible : *compatibles) {
      if (std::ranges::find(ArmDevicetreePsciItem::kCompatibleDevices, compatible) !=
          ArmDevicetreePsciItem::kCompatibleDevices.end()) {
        psci_ = &path.back();
        break;
      }
    }
  }

  return devicetree::ScanState::kActive;
}

std::optional<uint32_t> ArmDevicetreePsciCpuSuspendItem::IdleStateIndex(uint32_t phandle) const {
  if (phandle == 0) {
    return std::nullopt;
  }
  for (uint32_t i = 0; i < current_idle_state_; ++i) {
    if (state_phandles_[i] == phandle) {
      return i;
    }
  }
  return std::nullopt;
}

void ArmDevicetreePsciCpuSuspendItem::AddCpuToIdleStates(devicetree::ByteView phandles,
                                                         uint64_t cpu, bool from_power_domain) {
  for (size_t offset = 0; offset + sizeof(uint32_t) <= phandles.size();
       offset += sizeof(uint32_t)) {
    std::optional<uint32_t> phandle =
        devicetree::PropertyValue(phandles.subspan(offset, sizeof(uint32_t))).AsUint32();
    std::optional<uint32_t> index = phandle ? IdleStateIndex(*phandle) : std::nullopt;
    if (!index) {
      continue;
    }
    if (from_power_domain && IsPowerDomainState(states_[*index])) {
      continue;
    }
    state_cpus_[*index] |= cpu;
  }
}

void ArmDevicetreePsciCpuSuspendItem::AddCpuToPowerDomains(devicetree::ByteView phandles,
                                                           uint64_t cpu) {
  // Bounds the walk up the power domain hierarchy, which also guards against cycles.
  constexpr size_t kMaxPowerDomainDepth = 8;
  struct Frame {
    devicetree::ByteView phandles;
    size_t offset = 0;
  };
  std::array<Frame, kMaxPowerDomainDepth> stack{};
  stack[0] = {.phandles = phandles, .offset = 0};
  size_t depth = 0;

  while (true) {
    Frame& frame = stack[depth];
    if (frame.offset + sizeof(uint32_t) > frame.phandles.size()) {
      if (depth == 0) {
        break;
      }
      --depth;
      continue;
    }

    std::optional<uint32_t> phandle =
        devicetree::PropertyValue(frame.phandles.subspan(frame.offset, sizeof(uint32_t)))
            .AsUint32();
    frame.offset += sizeof(uint32_t);
    if (!phandle || *phandle == 0) {
      frame.offset = frame.phandles.size();
      continue;
    }

    PowerDomainEntry* matched = nullptr;
    for (uint32_t i = 0; i < current_power_domain_; ++i) {
      if (power_domains_[i].phandle == *phandle) {
        matched = &power_domains_[i];
        break;
      }
    }
    // A non-PSCI provider may have specifier cells; stop scanning the remaining cells.
    if (matched == nullptr) {
      if (depth == 0 && frame.offset == sizeof(uint32_t)) {
        Log("CPU `power-domains` does not start with a PSCI power domain; ignoring it.\n");
      }
      frame.offset = frame.phandles.size();
      continue;
    }

    matched->cpus |= cpu;
    AddCpuToIdleStates(matched->idle_states, cpu, /*from_power_domain=*/true);
    if (!matched->parents.empty()) {
      if (depth + 1 < kMaxPowerDomainDepth) {
        ++depth;
        stack[depth] = {.phandles = matched->parents, .offset = 0};
      } else {
        Log("Power domain hierarchy exceeds maximum depth (%zu); truncating.\n",
            kMaxPowerDomainDepth);
      }
    }
  }
}

void ArmDevicetreePsciCpuSuspendItem::MapCpus(DataZbi& zbi) {
  // The CPU topology item is appended before this one, so the last one is the shim's own.
  std::span<const zbi_topology_node_t> topology;
  for (auto [header, payload] : zbi) {
    if (header->type == ZBI_TYPE_CPU_TOPOLOGY) {
      topology = {reinterpret_cast<const zbi_topology_node_t*>(payload.data()),
                  payload.size() / sizeof(zbi_topology_node_t)};
    }
  }
  if (zbi.take_error().is_error()) {
    Log("Failed to iterate ZBI looking for ZBI_TYPE_CPU_TOPOLOGY.\n");
    return;
  }
  if (topology.empty()) {
    Log("No ZBI_TYPE_CPU_TOPOLOGY item; idle states apply to every CPU.\n");
    return;
  }

  for (uint32_t i = 0; i < current_cpu_; ++i) {
    const CpuEntry& cpu = cpus_[i];
    std::optional<uint16_t> logical_id;
    for (const zbi_topology_node_t& node : topology) {
      if (node.entity.discriminant != ZBI_TOPOLOGY_ENTITY_PROCESSOR) {
        continue;
      }
      const zbi_topology_processor_t& processor = node.entity.processor;
      if (processor.logical_id_count == 0 ||
          processor.architecture_info.discriminant != ZBI_TOPOLOGY_ARCHITECTURE_INFO_ARM64) {
        continue;
      }
      const zbi_topology_arm64_info_t& arm64 = processor.architecture_info.arm64;
      const uint64_t mpidr = (uint64_t{arm64.cluster_3_id} << 32u) |
                             (uint64_t{arm64.cluster_2_id} << 16u) |
                             (uint64_t{arm64.cluster_1_id} << 8u) | uint64_t{arm64.cpu_id};
      if (mpidr == cpu.mpidr) {
        logical_id = processor.logical_ids[0];
        break;
      }
    }
    if (!logical_id) {
      Log("CPU with MPIDR 0x%" PRIx64 " is not in ZBI_TYPE_CPU_TOPOLOGY; ignoring it.\n",
          cpu.mpidr);
      continue;
    }
    if (*logical_id >= 64) {
      Log("CPU with logical ID %" PRIu16 " cannot be described in idle state CPU masks.\n",
          *logical_id);
      continue;
    }
    const uint64_t bit = uint64_t{1} << *logical_id;
    AddCpuToIdleStates(cpu.idle_states, bit, /*from_power_domain=*/false);
    AddCpuToPowerDomains(cpu.power_domains, bit);
  }
}

bool ArmDevicetreePsciCpuSuspendItem::PowerDomainListsIdleState(const PowerDomainEntry& domain,
                                                                uint32_t index) const {
  for (size_t offset = 0; offset + sizeof(uint32_t) <= domain.idle_states.size();
       offset += sizeof(uint32_t)) {
    std::optional<uint32_t> phandle =
        devicetree::PropertyValue(domain.idle_states.subspan(offset, sizeof(uint32_t))).AsUint32();
    if (phandle && IdleStateIndex(*phandle) == index) {
      return true;
    }
  }
  return false;
}

fit::result<ArmDevicetreePsciCpuSuspendItem::DataZbi::Error>
ArmDevicetreePsciCpuSuspendItem::AppendItems(DataZbi& zbi) {
  // `num_idle_states_` is an upperbound on the number of possible valid idle states.
  // At this stage, `current_idle_state_` points to one past the last filled idle state, meaning
  // it accounts for possible invalid idle states being ignored, while `num_idle_states_` does not.
  if (current_idle_state_ == 0) {
    return fit::ok();
  }

  std::fill_n(state_cpus_, current_idle_state_, uint64_t{0});
  for (uint32_t i = 0; i < current_power_domain_; ++i) {
    power_domains_[i].cpus = 0;
  }
  MapCpus(zbi);

  // Calls `emit(index, cpus)` for each idle state entry to append, in order.
  auto for_each_entry = [this](auto&& emit) {
    for (uint32_t i = 0; i < current_idle_state_; ++i) {
      if (state_cpus_[i] != 0) {
        emit(i, state_cpus_[i]);
      }
      if (!IsPowerDomainState(states_[i])) {
        continue;
      }
      for (uint32_t d = 0; d < current_power_domain_; ++d) {
        const PowerDomainEntry& domain = power_domains_[d];
        if (domain.cpus != 0 && PowerDomainListsIdleState(domain, i)) {
          emit(i, domain.cpus);
        }
      }
    }
  };

  size_t count = 0;
  for_each_entry([&count](uint32_t, uint64_t) { ++count; });
  // If no CPU could be mapped to an idle state, every idle state applies to every CPU.
  const bool per_cpu = count != 0;
  if (!per_cpu) {
    count = current_idle_state_;
  }

  auto result = zbi.Append({
      .type = ZBI_TYPE_KERNEL_DRIVER,
      .length = static_cast<uint32_t>(count * sizeof(zbi_dcfg_arm_psci_cpu_suspend_state_t)),
      .extra = ZBI_KERNEL_DRIVER_ARM_PSCI_CPU_SUSPEND,
  });
  if (result.is_error()) {
    return result.take_error();
  }
  auto [header, payload_bytes] = **result;
  std::span payload(reinterpret_cast<zbi_dcfg_arm_psci_cpu_suspend_state_t*>(payload_bytes.data()),
                    count);

  uint32_t id = 0;
  auto emit = [&](uint32_t index, uint64_t cpu_mask) {
    zbi_dcfg_arm_psci_cpu_suspend_state_t& entry = payload[id];
    entry = states_[index];
    entry.id = id++;
    entry.cpu_mask = cpu_mask;
  };
  if (!per_cpu) {
    for (uint32_t i = 0; i < current_idle_state_; ++i) {
      emit(i, ~uint64_t{0});
    }
    return fit::ok();
  }

  for_each_entry(emit);
  for (uint32_t i = 0; i < current_idle_state_; ++i) {
    bool used = state_cpus_[i] != 0;
    for (uint32_t d = 0; !used && d < current_power_domain_; ++d) {
      used = IsPowerDomainState(states_[i]) && power_domains_[d].cpus != 0 &&
             PowerDomainListsIdleState(power_domains_[d], i);
    }
    if (!used) {
      Log("idle-state with power_state 0x%" PRIx32 " is not used by any CPU; skipping it.\n",
          states_[i].power_state);
    }
  }
  return fit::ok();
}

devicetree::ScanState ArmDevicetreePsciCpuSuspendItem::OnSubtree(const devicetree::NodePath& root) {
  if (&root.back() == idle_states_) {
    idle_states_ = nullptr;
  } else if (&root.back() == domain_idle_states_) {
    domain_idle_states_ = nullptr;
  } else if (&root.back() == psci_) {
    psci_ = nullptr;
  }
  return devicetree::ScanState::kActive;
}

devicetree::ScanState ArmDevicetreePsciCpuSuspendItem::OnScan() {
  if (!allocated() && num_idle_states_ > 0) {
    fbl::AllocChecker ac;
    states_ = reinterpret_cast<zbi_dcfg_arm_psci_cpu_suspend_state_t*>(
        (*allocator_)(num_idle_states_ * sizeof(zbi_dcfg_arm_psci_cpu_suspend_state_t),
                      alignof(zbi_dcfg_arm_psci_cpu_suspend_state_t), ac));
    if (ac.check()) {
      state_phandles_ = reinterpret_cast<uint32_t*>(
          (*allocator_)(num_idle_states_ * sizeof(uint32_t), alignof(uint32_t), ac));
    }
    if (ac.check()) {
      state_cpus_ = reinterpret_cast<uint64_t*>(
          (*allocator_)(num_idle_states_ * sizeof(uint64_t), alignof(uint64_t), ac));
    }
    if (ac.check() && num_cpus_ > 0) {
      cpus_ = reinterpret_cast<CpuEntry*>(
          (*allocator_)(num_cpus_ * sizeof(CpuEntry), alignof(CpuEntry), ac));
    }
    if (ac.check() && num_power_domains_ > 0) {
      power_domains_ = reinterpret_cast<PowerDomainEntry*>((*allocator_)(
          num_power_domains_ * sizeof(PowerDomainEntry), alignof(PowerDomainEntry), ac));
    }
    if (!ac.check()) {
      states_ = nullptr;
      state_phandles_ = nullptr;
      state_cpus_ = nullptr;
      cpus_ = nullptr;
      power_domains_ = nullptr;
      num_idle_states_ = 0;
      num_cpus_ = 0;
      num_power_domains_ = 0;
      OnError("Failed to allocate buffer for idle_states.");
      return devicetree::ScanState::kDone;
    }
    current_idle_state_ = 0;
    current_cpu_ = 0;
    current_power_domain_ = 0;
    return devicetree::ScanState::kActive;
  }
  return devicetree::ScanState::kDone;
}

}  // namespace boot_shim
