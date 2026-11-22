// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Per-processor CPU power state tracking and transition request generation.

use core::mem::{align_of, size_of};
use core::sync::atomic::Ordering;

use fbl::RefPtr;
use zx_status::Status;

use super::energy_model::{POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT, PowerDomain, PowerDomainSet};
pub use super::power_level_controller::PowerLevelUpdateRequest;
use super::{ProcessingRate, Utilization};

/// PowerState encapsulates the current active power level, current idle state index, power domain,
/// and energy model for an individual processor.
///
/// Instances of PowerState are not safe for concurrent use and must be protected by an external
/// lock associated with the owning processor.
#[repr(C)]
#[derive(Default)]
pub struct PowerState {
    /// The power domain set configured for this processor.
    power_domain_set: PowerDomainSet,

    /// The power domain the processor belongs to.
    domain: Option<RefPtr<PowerDomain>>,

    /// The 0-based active power level index (`0..active_levels().len()`) when the processor is
    /// actively running.
    active_power_level: Option<u8>,

    /// The 0-based active power level index that needs to be transitioned to.
    desired_active_power_level: Option<u8>,

    /// The 0-based idle state index (`0..idle_states().len()`) for this processor (`0` = shallowest
    /// idle state, typically WFI).
    idle_state_index: u8,

    /// The current normalized utilization of the processor.
    normalized_utilization: Utilization,
}

zr::static_assert!(size_of::<PowerState>() == 56);
zr::static_assert!(align_of::<PowerState>() == 8);

impl PowerState {
    /// Creates a new default `PowerState`.
    #[inline]
    pub const fn new() -> Self {
        Self {
            power_domain_set: PowerDomainSet::new(),
            domain: None,
            active_power_level: None,
            desired_active_power_level: None,
            idle_state_index: 0,
            normalized_utilization: Utilization::from_raw(0),
        }
    }

    /// Returns true if the power domain is set.
    #[inline]
    pub const fn is_enabled(&self) -> bool {
        self.domain().is_some()
    }

    /// Domain the PowerState is being modeled after. Returns `None` if there is
    /// no power domain configured for processor owning this power state.
    #[inline]
    pub const fn domain(&self) -> &Option<RefPtr<PowerDomain>> {
        &self.domain
    }

    /// Returns the id of the domain this power state belongs to.
    #[inline]
    pub fn domain_id(&self) -> Option<u32> {
        if let Some(domain) = self.domain() {
            return Some(domain.id());
        }
        None
    }

    /// Returns whether the domain's controller is serving requests. Returns false if there is no
    /// domain.
    #[inline]
    pub fn is_serving(&self) -> bool {
        self.domain().as_ref().is_some_and(|domain| domain.controller().is_serving())
    }

    /// Returns whether the kernel scheduler should send power level update requests through the
    /// control interface to handle utilization changes.
    #[inline]
    pub fn scheduler_control_enabled(&self) -> bool {
        self.domain().as_ref().is_some_and(|domain| domain.scheduler_control_enabled())
    }

    /// Returns the 0-based active power level index when the processor is not idle.
    #[inline]
    pub const fn active_power_level(&self) -> Option<u8> {
        self.active_power_level
    }

    /// Returns the 0-based active power level index the processor is in the process of
    /// transitioning to.
    #[inline]
    pub const fn desired_active_power_level(&self) -> Option<u8> {
        self.desired_active_power_level
    }

    /// Returns the current 0-based idle state index (`0` = shallowest idle state).
    #[inline]
    pub const fn idle_state_index(&self) -> u8 {
        self.idle_state_index
    }

    /// Returns the active power of the current active power level in nanowatts. Returns zero if
    /// there is no active power level or no domain is set.
    #[inline]
    pub fn active_power_coefficient_nw(&self) -> u64 {
        if let (Some(domain), Some(active_power_level)) = (self.domain(), self.active_power_level) {
            return domain.model().active_levels()[active_power_level as usize].active_power_nw();
        }
        0
    }

    /// Returns the steady-state idle power consumption in nanowatts at the current
    /// `idle_state_index` and `active_power_level`. Returns zero if no domain is set, no idle
    /// state is configured, or the current idle state is `ClockGated` and `active_power_level` is
    /// `None`.
    #[inline]
    pub fn idle_power_nw(&self) -> u64 {
        if let Some(domain) = self.domain() {
            return domain.model().idle_power_nw(
                self.idle_state_index as usize,
                self.active_power_level.map(usize::from),
            );
        }
        0
    }

    /// Returns true if the given 0-based power level index is a valid active power level
    /// (`IsValidActivePowerLevel`).
    #[inline]
    pub fn is_valid_active_power_level(&self, power_level: u8) -> bool {
        if let Some(domain) = self.domain() {
            return (power_level as usize) < domain.model().active_levels().len();
        }
        false
    }

    /// Returns the processing rate of the fastest power level in this domain.
    #[inline]
    pub fn max_processing_rate(&self) -> ProcessingRate {
        if let Some(domain) = self.domain() {
            return domain.model().max_processing_rate();
        }
        ProcessingRate::from_raw(0)
    }

    /// Returns the processing rate of the current active power level. Returns zero if there is no
    /// active power level or no domain is set.
    #[inline]
    pub fn active_processing_rate(&self) -> ProcessingRate {
        if let (Some(domain), Some(active_power_level)) = (self.domain(), self.active_power_level) {
            return domain.model().active_levels()[active_power_level as usize].processing_rate();
        }
        ProcessingRate::from_raw(0)
    }

    /// Returns the processing rate of the preceding active power level (i.e. the
    /// lower bound of the current active power level). Returns zero if there is no
    /// preceding active power level, no active power level is set, or no domain
    /// is set.
    #[inline]
    pub fn preceding_active_processing_rate(&self) -> ProcessingRate {
        if let (Some(domain), Some(active_power_level)) = (self.domain(), self.active_power_level) {
            let index = active_power_level as usize;
            if index > 0 {
                return domain.model().active_levels()[index - 1].processing_rate();
            }
        }
        ProcessingRate::from_raw(0)
    }

    /// Returns the targeted active power level (the in-flight desired level if set,
    /// otherwise the current active level).
    #[inline]
    pub fn target_active_power_level(&self) -> Option<u8> {
        self.desired_active_power_level.or(self.active_power_level)
    }

    /// Returns the processing rate of the targeted active power level (the in-flight
    /// desired level if set, otherwise the current active level).
    #[inline]
    pub fn target_processing_rate(&self) -> ProcessingRate {
        if let Some(domain) = self.domain()
            && let Some(level) = self.target_active_power_level()
        {
            return domain.model().active_levels()[level as usize].processing_rate();
        }
        ProcessingRate::from_raw(0)
    }

    /// Returns the processing rate of the active power level immediately preceding
    /// the targeted active power level (i.e. the lower bound of the targeted active
    /// power level).
    #[inline]
    pub fn preceding_target_processing_rate(&self) -> ProcessingRate {
        if let Some(domain) = self.domain()
            && let Some(level) = self.target_active_power_level()
        {
            let index = level as usize;
            if index > 0 {
                return domain.model().active_levels()[index - 1].processing_rate();
            }
        }
        ProcessingRate::from_raw(0)
    }

    /// Returns true if the targeted active power level has an active power level
    /// preceding it (i.e. is not the lowest active power level).
    #[inline]
    pub fn has_preceding_target_power_level(&self) -> bool {
        if self.domain().is_some()
            && let Some(level) = self.target_active_power_level()
        {
            return level > 0;
        }
        false
    }

    /// Returns the current utilization of the processor.
    #[inline]
    pub const fn normalized_utilization(&self) -> Utilization {
        self.normalized_utilization
    }

    /// Returns the total normalized utilization of the domain this processor belongs to.
    #[inline]
    pub fn total_normalized_utilization(&self) -> Utilization {
        if let Some(domain) = self.domain() {
            return domain.total_normalized_utilization();
        }
        Utilization::from_raw(0)
    }

    /// Updates the power domain set and associates this power state with the domain in the set
    /// matching the given CPU number, swapping `power_domain_set` with the previous power domain
    /// set in place (`UpdatePowerDomainSet`).
    pub fn update_power_domain_set(&mut self, power_domain_set: &mut PowerDomainSet, cpu_num: u32) {
        let domain = power_domain_set.find_by_cpu_num(cpu_num).cloned();

        if let Some(current_domain) = &self.domain {
            current_domain
                .total_normalized_utilization_atomic()
                .fetch_sub(self.normalized_utilization.raw_value(), Ordering::Relaxed);
        }

        self.active_power_level = None;
        self.desired_active_power_level = None;
        self.idle_state_index = 0;
        self.domain = domain;

        if let Some(domain) = &self.domain {
            let control_argument_result = domain.controller().get_current_power_level(domain.id());
            if let Ok(control_argument) = control_argument_result {
                let level = domain.model().find_active_power_level(
                    domain.controller().control_interface(),
                    control_argument,
                );
                self.active_power_level = level;
                self.desired_active_power_level = level;
            }

            domain
                .total_normalized_utilization_atomic()
                .fetch_add(self.normalized_utilization.raw_value(), Ordering::Relaxed);
        }

        core::mem::swap(power_domain_set, &mut self.power_domain_set);
    }

    /// Returns the power domain set configured for this processor.
    #[inline]
    pub const fn power_domain_set(&self) -> &PowerDomainSet {
        &self.power_domain_set
    }

    /// Posts a request to transition the power domain associated with this power state to a given
    /// 0-based active power level from the current energy model (`RequestTransition`).
    ///
    /// Ignored when no domain is set. Asserts that `active_power_level` is in bounds of
    /// `domain.model().active_levels()`.
    ///
    /// `cpu_num` is the CPU associated with this power state.
    /// `active_power_level` is the desired 0-based active power level index.
    pub fn request_transition(
        &mut self,
        cpu_num: u32,
        active_power_level: u8,
    ) -> Option<PowerLevelUpdateRequest> {
        let Some(domain) = &self.domain else {
            return None;
        };

        self.active_power_level?;

        let model = domain.model();
        assert!((active_power_level as usize) < model.active_levels().len(), "ASSERT FAILED");

        if self.desired_active_power_level == Some(active_power_level) {
            return None;
        }

        self.desired_active_power_level = Some(active_power_level);

        let level = &model.active_levels()[active_power_level as usize];

        let mut transition = PowerLevelUpdateRequest {
            domain_id: domain.id(),
            target_id: domain.id(),
            control: level.control(),
            control_argument: level.control_argument(),
            options: 0,
        };

        if level.targets_cpus() {
            transition.target_id = cpu_num;
            transition.options |= POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT as u32;
        }

        Some(transition)
    }

    /// Updates the 0-based active power level of this power state (`UpdateActivePowerLevel`).
    /// Typically called in response to the power level controller completing a power level
    /// transition request.
    pub fn update_active_power_level(&mut self, level: u8) -> Result<(), Status> {
        let Some(domain) = &self.domain else {
            return Err(Status::BAD_STATE);
        };

        if (level as usize) >= domain.model().active_levels().len() {
            return Err(Status::OUT_OF_RANGE);
        }

        let was_at_desired = self.desired_active_power_level == self.active_power_level;
        self.active_power_level = Some(level);
        if was_at_desired || self.desired_active_power_level == Some(level) {
            self.desired_active_power_level = Some(level);
        }
        Ok(())
    }

    /// Update the utilization of this processor and the total utilization of its domain
    /// (`UpdateUtilization`).
    #[inline]
    pub fn update_utilization(&mut self, utilization_delta: Utilization) -> Utilization {
        if let Some(domain) = self.domain() {
            domain
                .total_normalized_utilization_atomic()
                .fetch_add(utilization_delta.raw_value(), Ordering::Relaxed);
        }
        self.normalized_utilization += utilization_delta;
        self.normalized_utilization
    }
}

/// Unit tests for per-processor power state tracking.
#[cfg(test)]
mod tests {

    use super::super::ControlInterface;
    use super::super::energy_model::{
        ActivePowerLevelConfig, EnergyModel, IdlePowerModel, IdlePowerStateConfig, IdleStateFlags,
        POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT, PowerDomain, PowerDomainConfig, PowerDomainSet,
    };
    use super::PowerState;

    /// Verifies that domain-independent active levels target `cpu_num`.
    #[test]
    fn domain_independent_active_transition_targets_cpu() {
        let active_levels = [
            ActivePowerLevelConfig {
                options: POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT,
                processing_rate: 100,
                active_power_nw: 1_000_000,
                static_power_nw: 100_000,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 10,
                diagnostic_name: "opp0",
            },
            ActivePowerLevelConfig {
                options: POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT,
                processing_rate: 200,
                active_power_nw: 2_000_000,
                static_power_nw: 200_000,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 20,
                diagnostic_name: "opp1",
            },
        ];
        let config = PowerDomainConfig {
            domain_id: 123,
            cpu_mask: 0x0f,
            active_levels: &active_levels,
            idle_states: &[],
            active_transitions: &[],
        };
        let mut model = EnergyModel::create_from_config(&config, 0).expect("valid model");
        let domain = PowerDomain::create_for_test(123, 0x0f, &mut model).expect("valid domain");
        let mut domain_set = PowerDomainSet::create_for_test(Some(&domain));

        let mut state = PowerState::new();
        state.update_power_domain_set(&mut domain_set, 2);
        assert!(state.update_active_power_level(0).is_ok());

        let req = state.request_transition(2, 1).expect("expected transition request");
        assert_eq!(req.domain_id, 123);
        assert_eq!(req.target_id, 2);
        assert_eq!(req.control_argument, 20);
        assert_eq!(req.options, POWER_LEVEL_OPTIONS_DOMAIN_INDEPENDENT as u32);
    }

    /// Verifies `PowerState::idle_power_nw` across `ClockGated` and `PowerGated` states.
    #[test]
    fn idle_power_nw_tracks_opp_and_power_gated_residual() {
        let active_levels = [
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 100,
                active_power_nw: 1_000_000,
                static_power_nw: 150_000,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 0,
                diagnostic_name: "opp0",
            },
            ActivePowerLevelConfig {
                options: 0,
                processing_rate: 200,
                active_power_nw: 2_500_000,
                static_power_nw: 400_000,
                control_interface: ControlInterface::CpuDriver,
                control_argument: 1,
                diagnostic_name: "opp1",
            },
        ];
        let clock_gated_idle = [IdlePowerStateConfig::ARM_WFI];
        let power_gated_idle = [IdlePowerStateConfig {
            power_model: IdlePowerModel::PowerGated { residual_power_nw: 25_000 },
            entry_latency_ns: 1_000,
            exit_latency_ns: 2_000,
            min_residency_ns: 10_000,
            scope_cpu_mask: 0x03,
            flags: IdleStateFlags::LOCAL_TIMER_STOPS,
            control_interface: ControlInterface::ArmPsci,
            control_argument: 0x0100_0001,
            diagnostic_name: "c2",
        }];

        let mut state = PowerState::new();
        let set_domain_idle_states =
            |state: &mut PowerState, domain_id: u32, idle_states: &[IdlePowerStateConfig<'_>]| {
                let mut model = EnergyModel::create_from_config(
                    &PowerDomainConfig {
                        domain_id,
                        cpu_mask: 0x03,
                        active_levels: &active_levels,
                        idle_states,
                        active_transitions: &[],
                    },
                    0,
                )
                .expect("valid energy model");
                let domain = PowerDomain::create_for_test(domain_id, 0x03, &mut model)
                    .expect("valid domain");
                let mut domain_set = PowerDomainSet::create_for_test(Some(&domain));
                state.update_power_domain_set(&mut domain_set, 0);
            };

        // ClockGated idle state tracks the active OPP's static_power_nw.
        set_domain_idle_states(&mut state, 1, &clock_gated_idle);
        assert_eq!(state.idle_state_index(), 0);
        assert_eq!(state.idle_power_nw(), 0);

        assert!(state.update_active_power_level(0).is_ok());
        assert_eq!(state.idle_power_nw(), 150_000);
        assert!(state.update_active_power_level(1).is_ok());
        assert_eq!(state.idle_power_nw(), 400_000);

        // PowerGated idle state reports residual_power_nw regardless of active OPP.
        set_domain_idle_states(&mut state, 2, &power_gated_idle);
        assert_eq!(state.idle_power_nw(), 25_000);
        assert!(state.update_active_power_level(0).is_ok());
        assert_eq!(state.idle_power_nw(), 25_000);
        assert!(state.update_active_power_level(1).is_ok());
        assert_eq!(state.idle_power_nw(), 25_000);

        // Empty idle_states slice reports 0 nW.
        set_domain_idle_states(&mut state, 3, &[]);
        assert!(state.update_active_power_level(1).is_ok());
        assert_eq!(state.idle_power_nw(), 0);
    }
}
