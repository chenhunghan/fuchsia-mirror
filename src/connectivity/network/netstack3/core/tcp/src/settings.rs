// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::num::NonZeroUsize;

use netstack3_base::BufferSizeSettings;

/// TCP layer stack settings.
#[derive(Clone)]
pub struct TcpSettings {
    /// Receive buffer settings.
    pub receive_buffer: BufferSizeSettings<NonZeroUsize>,
    /// Send buffer settings.
    pub send_buffer: BufferSizeSettings<NonZeroUsize>,
    /// Whether TCP Generic Segmentation Offload (GSO) is enabled.
    ///
    /// When enabled, TCP may emit segments carrying more than one MSS worth
    /// of payload, which are split into MSS-sized segments by the IP layer on
    /// egress.
    pub gso_enabled: bool,
}

#[cfg(any(test, feature = "testutils"))]
impl Default for TcpSettings {
    fn default() -> Self {
        // Arbitrarily chosen to satisfy tests so we have some semblance of
        // clamping capacity in tests.

        let min = NonZeroUsize::new(16).unwrap();
        let max = NonZeroUsize::new(16 << 20).unwrap();
        let default = NonZeroUsize::new(netstack3_base::WindowSize::DEFAULT.into()).unwrap();
        let sizes = BufferSizeSettings::new(min, default, max).unwrap();
        // TODO(https://fxbug.dev/565891068): Default `gso_enabled` to true.
        Self { receive_buffer: sizes, send_buffer: sizes, gso_enabled: false }
    }
}
