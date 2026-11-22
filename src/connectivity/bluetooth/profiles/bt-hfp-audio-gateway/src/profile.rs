// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context as _;
use fidl_fuchsia_bluetooth as fidl_bt;
use fidl_fuchsia_bluetooth_bredr as bredr;
use profile_client::ProfileClient;

use crate::config::AudioGatewayFeatureSupport;
use crate::service_definitions;

fn register(
    proxy: bredr::ProfileProxy,
    features: AudioGatewayFeatureSupport,
) -> anyhow::Result<ProfileClient> {
    // Register the service advertisement for the Audio Gateway role implemented by us.
    let service_definition = service_definitions::audio_gateway(features);
    let mut profile = ProfileClient::advertise(
        proxy,
        vec![service_definition],
        fidl_bt::ChannelParameters::default(),
    )?;

    // Register a search for remote peers that support the Hands Free role.
    profile.add_search(bredr::ServiceClassProfileIdentifier::Handsfree, None)?;

    Ok(profile)
}

/// Register the Audio Gateway profile. Returns a `ProfileClient` to interact
/// with the `bredr.Profile` and the ProfileProxy on success or Error on failure.
pub fn register_audio_gateway(
    features: AudioGatewayFeatureSupport,
) -> anyhow::Result<(ProfileClient, bredr::ProfileProxy)> {
    let proxy = fuchsia_component::client::connect_to_protocol::<bredr::ProfileMarker>()
        .context("Failed to connect to Bluetooth Profile service")?;
    let profile_client = register(proxy.clone(), features)?;
    Ok((profile_client, proxy))
}

#[cfg(test)]
pub(crate) mod test_server {
    use super::*;
    use test_profile_server::TestProfileServer;

    /// Register a new Profile object, and create an associated test server.
    pub(crate) fn setup_profile_and_test_server()
    -> (ProfileClient, bredr::ProfileProxy, TestProfileServer) {
        let (proxy, test_server) = TestProfileServer::create_endpoints();

        let profile = register(proxy.clone(), Default::default()).expect("register profile");
        (profile, proxy, test_server)
    }
}
