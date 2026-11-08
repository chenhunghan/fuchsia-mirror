// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fuchsia startup helper for the [`redactor`] crate.
//!
//! Components that log or record MAC addresses, BSSIDs, or SSIDs call
//! [`enable`] once at startup when redaction is configured. The process-global
//! redactor is switched to [`redactor::MaskingRedactor`] synchronously so no
//! raw identifier is ever formatted, and the returned [`Future`] fetches the
//! per-boot salt from `fuchsia.diagnostics.SaltProvider` and upgrades the
//! global redactor to [`redactor::SaltedRedactor`].

#![deny(missing_docs)]

use fidl_fuchsia_diagnostics::{SaltProviderMarker, SaltProviderProxy};
use fuchsia_component::client::connect_to_protocol;
use log::warn;
use std::future::Future;

/// Enables the process-global redactor.
///
/// [`redactor::MaskingRedactor`] is installed synchronously when `enable` is
/// called so that no raw identifier is ever formatted, even before the
/// returned [`Future`] is first polled. Polling the returned future connects to
/// `fuchsia.diagnostics.SaltProvider`, fetches the per-boot salt, and installs
/// a [`redactor::SaltedRedactor`] once it arrives.
///
/// Callers may `.await` the returned future directly, spawn it onto an
/// executor, or join it with other futures. If the future is dropped before
/// completing, identifiers stay masked rather than hashed.
pub fn enable() -> impl Future<Output = ()> {
    redactor::set_masking_redactor();
    async {
        match connect_to_protocol::<SaltProviderMarker>() {
            Ok(proxy) => init_with_proxy(proxy).await,
            Err(e) => {
                warn!(e:?; "Failed to connect to fuchsia.diagnostics.SaltProvider; \
                            network identifiers will remain masked");
            }
        }
    }
}

/// Fetches the salt from `proxy` and installs a [`redactor::SaltedRedactor`].
///
/// Exposed so tests can drive the transition with a fake `SaltProvider`.
pub async fn init_with_proxy(proxy: SaltProviderProxy) {
    match proxy.get_salt().await {
        Ok(salt) => {
            if let Err(e) = redactor::set_salted_redactor(salt) {
                warn!(e:?; "Global redactor already installed; ignoring fetched salt");
            }
        }
        Err(e) => {
            warn!(e:?; "Failed to fetch salt from fuchsia.diagnostics.SaltProvider; \
                        network identifiers will remain masked");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_diagnostics::SaltProviderRequest;
    use futures::StreamExt;
    use std::fmt;

    struct Mac([u8; redactor::MAC_LEN]);

    fn fmt_mac(m: [u8; redactor::MAC_LEN], f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
    }

    impl redactor::Redactable for Mac {
        fn fmt_unredacted(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt_mac(self.0, f)
        }

        fn fmt_redacted(
            &self,
            r: &dyn redactor::Redactor,
            f: &mut fmt::Formatter<'_>,
        ) -> fmt::Result {
            fmt_mac(r.redact_mac(self.0), f)?;
            f.write_str(redactor::REDACTED_MARKER)
        }
    }

    redactor::impl_display_via_redactable!(Mac);

    // The redactor state is process-global, so the whole transition is
    // exercised in a single test to avoid ordering dependencies between tests.
    #[fuchsia::test]
    async fn init_masks_then_salts() {
        const SALT: [u8; redactor::SALT_LEN] = [0x5a; redactor::SALT_LEN];
        let mac = Mac([0x00, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e]);

        // Not yet initialized: raw.
        assert_eq!(mac.to_string(), "00:1a:2b:3c:4d:5e");

        // Masking is applied synchronously when `enable()` is called, even
        // before the returned future is polled.
        let _unpolled_fut = enable();
        assert_eq!(mac.to_string(), "00:1a:2b:00:00:00*");

        let (proxy, mut stream) = create_proxy_and_stream::<SaltProviderMarker>();
        let server = async move {
            let SaltProviderRequest::GetSalt { responder } =
                stream.next().await.expect("request").expect("ok")
            else {
                panic!("unexpected request");
            };
            responder.send(&SALT).expect("send salt");
        };
        let ((), ()) = futures::join!(init_with_proxy(proxy), server);

        // Salted: the global now hashes with the salt the fake server handed
        // out, i.e. the same bytes an explicit `SaltedRedactor::new(SALT)`
        // would produce (OUI preserved, NIC hashed).
        let expected =
            Mac(redactor::Redactor::redact_mac(&redactor::SaltedRedactor::new(SALT), mac.0));
        assert_eq!(&expected.0[..3], &mac.0[..3]);
        assert_ne!(&expected.0[3..], &mac.0[3..]);
        assert_eq!(mac.to_string(), format!("{}", redactor::Unredacted(&expected)) + "*");
    }
}
