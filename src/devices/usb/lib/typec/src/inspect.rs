// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Inspect diagnostic recording for [`crate::TypecPort`].

use crate::port::{DebounceDirection, TypecPort};

const INSPECTOR_PROP_IS_CONNECTED: &str = "is_connected";
const INSPECTOR_PROP_LAST_UPDATED_TIME_NS: &str = "last_updated_time_ns";
const INSPECTOR_PROP_CC1_STATE: &str = "cc1_state";
const INSPECTOR_PROP_CC2_STATE: &str = "cc2_state";
const INSPECTOR_PROP_TYPEC_STATE: &str = "typec_state";
const INSPECTOR_PROP_ACTIVE_DEBOUNCE_DIRECTION: &str = "active_debounce_direction";
const INSPECTOR_PROP_ATTACH_COUNT: &str = "attach_count";
const INSPECTOR_PROP_DISCONNECT_COUNT: &str = "disconnect_count";
const INSPECTOR_PROP_DEBOUNCE_CANCELLED_COUNT: &str = "debounce_cancelled_count";
const INSPECTOR_PROP_SINK_RP_LEVEL: &str = "sink_rp_level";
const INSPECTOR_PROP_VBUS_DISCHARGE_ARMED: &str = "vbus_discharge_armed";
const INSPECTOR_PROP_VBUS_SINK_ENABLED: &str = "vbus_sink_enabled";
const INSPECTOR_PROP_PENDING_TIMER: &str = "pending_timer";
const INSPECTOR_PROP_RP_CHANGE_COUNT: &str = "rp_change_count";
const INSPECTOR_PROP_SINK_ATTACH_FAILURE_COUNT: &str = "sink_attach_failure_count";
const INSPECTOR_PROP_SOURCE_ATTACH_FAILURE_COUNT: &str = "source_attach_failure_count";
const INSPECTOR_PROP_DISCHARGE_TIMEOUT_COUNT: &str = "discharge_timeout_count";

/// Records `port`'s Type-C state and telemetry properties onto `node`.
pub(crate) fn record(port: &TypecPort, node: &fuchsia_inspect::Node) {
    let state = port.state.lock();
    let machine = &state.machine;
    let typec_state = machine.typec_state();
    node.record_bool(INSPECTOR_PROP_IS_CONNECTED, typec_state.is_attached());
    node.record_int(
        INSPECTOR_PROP_LAST_UPDATED_TIME_NS,
        machine.telemetry.last_updated_time.into_nanos(),
    );
    let cc = machine.visible_cc_status();
    node.record_string(INSPECTOR_PROP_CC1_STATE, cc.cc1.as_str());
    node.record_string(INSPECTOR_PROP_CC2_STATE, cc.cc2.as_str());
    node.record_string(INSPECTOR_PROP_TYPEC_STATE, typec_state.as_str());
    let direction = state.active_direction().map_or("none", DebounceDirection::as_str);
    node.record_string(INSPECTOR_PROP_ACTIVE_DEBOUNCE_DIRECTION, direction);
    if let Some(rp) = machine.sink_rp_level() {
        node.record_string(INSPECTOR_PROP_SINK_RP_LEVEL, rp.as_str());
    }
    node.record_bool(
        INSPECTOR_PROP_VBUS_DISCHARGE_ARMED,
        state.runner.hw.vbus_discharge.is_enabled(),
    );
    node.record_bool(INSPECTOR_PROP_VBUS_SINK_ENABLED, state.runner.hw.vbus_sink.is_enabled());
    node.record_string(INSPECTOR_PROP_PENDING_TIMER, state.pending_timer());
    let telemetry = &machine.telemetry;
    for (name, count) in [
        (INSPECTOR_PROP_ATTACH_COUNT, telemetry.attach_count),
        (INSPECTOR_PROP_DISCONNECT_COUNT, telemetry.disconnect_count),
        (INSPECTOR_PROP_DEBOUNCE_CANCELLED_COUNT, telemetry.debounce_cancelled_count),
        (INSPECTOR_PROP_RP_CHANGE_COUNT, telemetry.rp_change_count),
        (INSPECTOR_PROP_SINK_ATTACH_FAILURE_COUNT, telemetry.sink_attach_failure_count),
        (INSPECTOR_PROP_SOURCE_ATTACH_FAILURE_COUNT, telemetry.source_attach_failure_count),
        (INSPECTOR_PROP_DISCHARGE_TIMEOUT_COUNT, telemetry.discharge_timeout_count),
    ] {
        node.record_uint(name, count);
    }
}
