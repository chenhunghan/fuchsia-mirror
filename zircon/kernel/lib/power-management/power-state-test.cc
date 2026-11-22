// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "lib/power-management/power-state.h"

#include <lib/power-management/energy-model.h>

#include <cstdint>

#include <gtest/gtest.h>

#include "test-helper.h"

namespace {

using power_management::PowerDomainSet;
using power_management::PowerState;
using power_management::Utilization;

constexpr uint32_t kModelId = 123;
constexpr uint8_t kActiveLevels = 5;

constexpr Utilization kZeroUtilization{0};
constexpr Utilization kOneHalfUtilization = ffl::FromRatio(1, 2);
constexpr Utilization kOneQuarterUtilization = ffl::FromRatio(1, 4);

TEST(PowerStateTest, Default) {
  PowerState state;
  EXPECT_EQ(state.domain(), nullptr);
  EXPECT_FALSE(state.is_enabled());
  EXPECT_FALSE(state.domain_id());
  EXPECT_FALSE(state.is_serving());
  EXPECT_FALSE(state.active_power_level());
  EXPECT_FALSE(state.desired_active_power_level());
  EXPECT_EQ(0u, state.active_power_coefficient_nw());
  EXPECT_EQ(0u, state.idle_power_nw());
  EXPECT_EQ(ZX_ERR_BAD_STATE, state.UpdateActivePowerLevel(0).status_value());
  EXPECT_EQ(kZeroUtilization, state.normalized_utilization());
}

TEST(PowerStateTest, UpdateDomainSetsModel) {
  auto domain = MakePowerDomainHelper(kModelId, 0, 1, 2, 3, 4, 5);

  PowerState state;
  state.UpdatePowerDomainSet(PowerDomainSet::CreateForTest(domain), 0);

  EXPECT_EQ(state.domain(), domain);
  EXPECT_EQ(state.domain_id(), kModelId);
  EXPECT_FALSE(state.active_power_level());
  EXPECT_FALSE(state.desired_active_power_level());
  EXPECT_EQ(0u, state.active_power_coefficient_nw());
  // Clock-gated WFI without a known active OPP reports 0 nW.
  EXPECT_EQ(0u, state.idle_power_nw());
}

TEST(PowerStateTest, IdlePowerTracksActiveStaticPowerAndEmptyIdleStates) {
  auto energy_model = MakeFakeEnergyModel(kActiveLevels, 3);
  auto domain = MakePowerDomainHelper(kModelId, energy_model, 0, 1);

  PowerState state;
  state.UpdatePowerDomainSet(PowerDomainSet::CreateForTest(domain), 0);

  // State 0 is ClockGated WFI: idle_power_nw() tracks the active OPP's static_power_nw.
  ASSERT_EQ(ZX_OK, state.UpdateActivePowerLevel(0).status_value());
  EXPECT_EQ(ActivePowerNwForLevel(0), state.active_power_coefficient_nw());
  EXPECT_EQ(StaticPowerNwForLevel(0), state.idle_power_nw());

  ASSERT_EQ(ZX_OK, state.UpdateActivePowerLevel(3).status_value());
  EXPECT_EQ(ActivePowerNwForLevel(3), state.active_power_coefficient_nw());
  EXPECT_EQ(StaticPowerNwForLevel(3), state.idle_power_nw());

  // Domains with no configured idle states report 0 nW.
  auto empty_idle_model = MakeFakeEnergyModel(kActiveLevels, 0);
  auto empty_idle_domain = MakePowerDomainHelper(kModelId + 1, empty_idle_model, 0, 1);
  state.UpdatePowerDomainSet(PowerDomainSet::CreateForTest(empty_idle_domain), 0);
  ASSERT_EQ(ZX_OK, state.UpdateActivePowerLevel(3).status_value());
  EXPECT_EQ(0u, state.idle_power_nw());
}

TEST(PowerStateTest, UpdateDomainKeepsPowerLevelWhenSameModelId) {
  auto domain = MakePowerDomainHelper(kModelId, 0, 1, 2, 3, 4, 5);
  auto domain2 = MakePowerDomainHelper(kModelId, 0, 1, 2, 3, 4);

  PowerState state;
  state.UpdatePowerDomainSet(PowerDomainSet::CreateForTest(domain), 0);
  ASSERT_EQ(state.domain(), domain);
  ASSERT_EQ(ZX_OK, state.UpdateActivePowerLevel(1).status_value());

  static_cast<FakePowerLevelController*>(domain2->controller().get())->current_power_level = 0;

  state.UpdatePowerDomainSet(PowerDomainSet::CreateForTest(domain2), 0);
  EXPECT_EQ(state.domain(), domain2);
  EXPECT_EQ(state.active_power_level(), 0u);
  EXPECT_EQ(state.desired_active_power_level(), 0u);
}

TEST(PowerStateTest, UpdateDomainClearsPowerLevelWhenDifferentModelId) {
  auto domain = MakePowerDomainHelper(kModelId, 0, 1, 2, 3, 4, 5);
  auto domain2 = MakePowerDomainHelper(kModelId + 1, 0, 1, 2, 3, 4, 5);

  PowerState state;
  state.UpdatePowerDomainSet(PowerDomainSet::CreateForTest(domain), 0);
  ASSERT_EQ(state.domain(), domain);
  ASSERT_EQ(ZX_OK, state.UpdateActivePowerLevel(2).status_value());

  static_cast<FakePowerLevelController*>(domain2->controller().get())->current_power_level = 1;

  state.UpdatePowerDomainSet(PowerDomainSet::CreateForTest(domain2), 0);
  EXPECT_EQ(state.domain(), domain2);
  EXPECT_EQ(state.active_power_level(), 1u);
  EXPECT_EQ(state.desired_active_power_level(), 1u);
}

TEST(PowerStateTest, TransitionWhenModelIsUnknown) {
  PowerState state;
  EXPECT_FALSE(state.RequestTransition(1, 0));
}

TEST(PowerStateTest, TransitionWhenPowerLevelIsUnknown) {
  auto energy_model = MakeFakeEnergyModel(kActiveLevels);
  auto domain = MakePowerDomainHelper(kModelId, energy_model, 0, 1, 2, 3, 4, 5);

  PowerState state;
  state.UpdatePowerDomainSet(PowerDomainSet::CreateForTest(domain), 0);
  ASSERT_EQ(state.domain(), domain);

  EXPECT_FALSE(state.RequestTransition(1, 1));
}

TEST(PowerStateTest, TransitionWhenPowerLevelIsDesiredPowerLevel) {
  auto energy_model = MakeFakeEnergyModel(kActiveLevels);
  auto domain = MakePowerDomainHelper(kModelId, energy_model, 0, 1, 2, 3, 4, 5);

  PowerState state;
  state.UpdatePowerDomainSet(PowerDomainSet::CreateForTest(domain), 0);
  ASSERT_EQ(state.domain(), domain);
  ASSERT_EQ(ZX_OK, state.UpdateActivePowerLevel(0).status_value());

  EXPECT_FALSE(state.RequestTransition(1, 0));
}

TEST(PowerStateTest, TransitionWhenPowerLevelIsAlreadyInFlight) {
  auto energy_model = MakeFakeEnergyModel(kActiveLevels);
  auto domain = MakePowerDomainHelper(kModelId, energy_model, 0, 1, 2, 3, 4, 5);

  PowerState state;
  state.UpdatePowerDomainSet(PowerDomainSet::CreateForTest(domain), 0);
  ASSERT_EQ(state.domain(), domain);
  ASSERT_EQ(ZX_OK, state.UpdateActivePowerLevel(0).status_value());

  // First request for a higher level succeeds and enters in-flight state.
  EXPECT_TRUE(state.RequestTransition(1, 1));
  EXPECT_EQ(state.desired_active_power_level(), 1u);

  // Subsequent request for the same level is suppressed.
  EXPECT_FALSE(state.RequestTransition(1, 1));

  // Request for an even higher level updates the desired level and succeeds.
  EXPECT_TRUE(state.RequestTransition(1, 2));
  EXPECT_EQ(state.desired_active_power_level(), 2u);

  // Acknowledging the transition updates active and desired levels.
  ASSERT_EQ(ZX_OK, state.UpdateActivePowerLevel(2).status_value());
  EXPECT_EQ(state.active_power_level(), 2u);
  EXPECT_EQ(state.desired_active_power_level(), 2u);
  EXPECT_FALSE(state.RequestTransition(1, 2));
}

TEST(PowerStateTest, TransitionAndUpdateOutOfRange) {
  auto energy_model = MakeFakeEnergyModel(kActiveLevels);
  auto domain = MakePowerDomainHelper(kModelId, energy_model, 0, 1, 2, 3, 4, 5);

  PowerState state;
  state.UpdatePowerDomainSet(PowerDomainSet::CreateForTest(domain), 0);
  ASSERT_EQ(state.domain(), domain);
  ASSERT_EQ(ZX_OK, state.UpdateActivePowerLevel(0).status_value());

  EXPECT_TRUE(state.IsValidActivePowerLevel(0));
  EXPECT_TRUE(state.IsValidActivePowerLevel(kActiveLevels - 1));
  EXPECT_FALSE(state.IsValidActivePowerLevel(kActiveLevels));
  EXPECT_EQ(ZX_ERR_OUT_OF_RANGE, state.UpdateActivePowerLevel(kActiveLevels).status_value());

  EXPECT_DEATH(state.RequestTransition(1, kActiveLevels), "ASSERT FAILED");
}

TEST(PowerStateTest, UpdateUtilizationReflectsOnDomain) {
  auto energy_model = MakeFakeEnergyModel(kActiveLevels);
  auto energy_model2 = MakeFakeEnergyModel(kActiveLevels);
  auto domain = MakePowerDomainHelper(kModelId, energy_model, 0, 1, 2, 3, 4, 5);
  auto domain2 = MakePowerDomainHelper(kModelId + 1, energy_model2, 0, 1, 2, 3, 4, 5);

  PowerState state;

  EXPECT_EQ(kZeroUtilization, state.normalized_utilization());
  EXPECT_EQ(kZeroUtilization, domain->total_normalized_utilization());
  EXPECT_EQ(kZeroUtilization, domain2->total_normalized_utilization());

  // Update utilization before associating with a domain.
  state.UpdateUtilization(kOneHalfUtilization);

  EXPECT_EQ(kOneHalfUtilization, state.normalized_utilization());
  EXPECT_EQ(kZeroUtilization, domain->total_normalized_utilization());
  EXPECT_EQ(kZeroUtilization, domain2->total_normalized_utilization());

  // Associating with a domain should update the domain total.
  state.UpdatePowerDomainSet(PowerDomainSet::CreateForTest(domain), 0);
  ASSERT_EQ(state.domain(), domain);

  EXPECT_EQ(kOneHalfUtilization, domain->total_normalized_utilization());
  EXPECT_EQ(kZeroUtilization, domain2->total_normalized_utilization());

  // Update utilization while associated with a domain.
  state.UpdateUtilization(-kOneQuarterUtilization);

  EXPECT_EQ(kOneQuarterUtilization, state.normalized_utilization());
  EXPECT_EQ(kOneQuarterUtilization, domain->total_normalized_utilization());
  EXPECT_EQ(kZeroUtilization, domain2->total_normalized_utilization());

  // Changing to a different domain should move the utilization from the
  // previous domain to the new domain.
  state.UpdatePowerDomainSet(PowerDomainSet::CreateForTest(domain2), 0);

  EXPECT_EQ(kOneQuarterUtilization, state.normalized_utilization());
  EXPECT_EQ(kZeroUtilization, domain->total_normalized_utilization());
  EXPECT_EQ(kOneQuarterUtilization, domain2->total_normalized_utilization());
}

TEST(PowerStateTest, PrecedingTargetAndActivePowerLevel) {
  auto energy_model = MakeFakeEnergyModel(kActiveLevels);
  auto domain = MakePowerDomainHelper(kModelId, energy_model, 0, 1, 2, 3, 4, 5);

  PowerState state;
  state.UpdatePowerDomainSet(PowerDomainSet::CreateForTest(domain), 0);
  ASSERT_EQ(state.domain(), domain);
  ASSERT_EQ(ZX_OK, state.UpdateActivePowerLevel(0).status_value());

  // At active index 0, there is no preceding active level.
  const auto level0_rate = state.active_processing_rate();
  EXPECT_GT(level0_rate, power_management::ProcessingRate{0});
  EXPECT_FALSE(state.has_preceding_target_power_level());
  EXPECT_EQ(power_management::ProcessingRate{0}, state.preceding_target_processing_rate());
  EXPECT_EQ(power_management::ProcessingRate{0}, state.preceding_active_processing_rate());

  // At active index 1, the preceding active level is index 0.
  ASSERT_EQ(ZX_OK, state.UpdateActivePowerLevel(1).status_value());
  EXPECT_TRUE(state.has_preceding_target_power_level());
  EXPECT_EQ(level0_rate, state.preceding_target_processing_rate());
  EXPECT_EQ(level0_rate, state.preceding_active_processing_rate());

  // While a transition from index 1 to index 3 is in flight, target precedes index 3 (level 2)
  // whereas active still precedes index 1 (level 0).
  ASSERT_EQ(ZX_OK, state.UpdateActivePowerLevel(2).status_value());
  const auto level2_rate = state.active_processing_rate();
  ASSERT_EQ(ZX_OK, state.UpdateActivePowerLevel(1).status_value());
  ASSERT_TRUE(state.RequestTransition(1, 3));
  EXPECT_TRUE(state.has_preceding_target_power_level());
  EXPECT_EQ(level2_rate, state.preceding_target_processing_rate());
  EXPECT_EQ(level0_rate, state.preceding_active_processing_rate());
}

TEST(PowerStateTest, SchedulerControlEnabled) {
  auto domain = MakePowerDomainHelper(kModelId, 0, 1, 2, 3, 4, 5);

  PowerState state;
  EXPECT_FALSE(state.scheduler_control_enabled());

  state.UpdatePowerDomainSet(PowerDomainSet::CreateForTest(domain), 0);
  EXPECT_FALSE(state.scheduler_control_enabled());

  domain->SetSchedulerControlEnabled(true);
  EXPECT_TRUE(state.scheduler_control_enabled());

  domain->SetSchedulerControlEnabled(false);
  EXPECT_FALSE(state.scheduler_control_enabled());
}

TEST(PowerStateTest, UpdateDomainWithUnknownControlArgumentLeavesLevelUnset) {
  auto domain = MakePowerDomainHelper(kModelId, 0, 1);
  static_cast<FakePowerLevelController*>(domain->controller().get())->current_power_level = 99;

  PowerState state;
  state.UpdatePowerDomainSet(PowerDomainSet::CreateForTest(domain), 0);
  EXPECT_FALSE(state.active_power_level());
  EXPECT_FALSE(state.desired_active_power_level());
  EXPECT_EQ(0u, state.idle_power_nw());
  EXPECT_FALSE(state.RequestTransition(0, 0));
}

TEST(EnergyModelWrapperTest, FindActivePowerLevelAndDomainLookups) {
  auto model = power_management::EnergyModel::CreateForTest(/*idle_states=*/2, /*active_levels=*/3);
  EXPECT_EQ(model.FindActivePowerLevelForRate(power_management::ProcessingRate{0}), 0u);
  EXPECT_EQ(model.FindActivePowerLevel(power_management::ControlInterface::kCpuDriver, 2), 2u);
  EXPECT_FALSE(model.FindActivePowerLevel(power_management::ControlInterface::kCpuDriver, 99));
  EXPECT_FALSE(model.FindActivePowerLevel(static_cast<power_management::ControlInterface>(495), 0));

  auto domain = MakePowerDomainHelper(kModelId, 0, 1);
  auto domain_set = PowerDomainSet::CreateForTest(domain);
  EXPECT_EQ(domain_set.count(), 1u);
  EXPECT_EQ(domain_set.FindByDomainId(kModelId), domain.get());
  EXPECT_EQ(domain_set.FindByDomainId(kModelId + 1), nullptr);
}

}  // namespace
