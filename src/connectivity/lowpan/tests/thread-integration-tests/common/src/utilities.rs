// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Helpers shared by suites of tests using the fixture.

use fidl_fuchsia_lowpan_device as flowpan_device;

/// The IEEE 802.15.4 channel of the network formed in the tests.
pub const CHANNEL: u8 = 15;

/// Parameters of the network formed in the tests.
pub fn provisioning_params() -> flowpan_device::ProvisioningParams {
    flowpan_device::ProvisioningParams {
        identity: flowpan_device::Identity {
            raw_name: Some(b"thread-it".to_vec()),
            net_type: Some(flowpan_device::NET_TYPE_THREAD_1_X.to_string()),
            channel: Some(CHANNEL.into()),
            panid: Some(0x1234),
            ..Default::default()
        },
        credential: Some(Box::new(flowpan_device::Credential::NetworkKey(
            (0..16).collect::<Vec<u8>>(),
        ))),
    }
}
