// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::CapabilityBound;
#[cfg(target_os = "fuchsia")]
use crate::Receiver;
#[cfg(target_os = "fuchsia")]
use futures::channel::mpsc;
use std::fmt::Debug;
use std::sync::Arc;

/// Types that implement [`Connectable`] let the holder send channels
/// to them.
#[cfg(target_os = "fuchsia")]
pub trait Connectable: Send + Sync + Debug {
    fn send(&self, channel: zx::Channel) -> Result<(), ()>;
}

#[cfg(not(target_os = "fuchsia"))]
pub trait Connectable: Send + Sync + Debug {}

#[cfg(target_os = "fuchsia")]
impl Connectable for mpsc::UnboundedSender<zx::Channel> {
    fn send(&self, channel: zx::Channel) -> Result<(), ()> {
        self.unbounded_send(channel).map_err(|_| ())
    }
}

/// A capability that transfers another capability to a [Receiver].
#[derive(Debug)]
pub struct Connector {
    #[cfg(target_os = "fuchsia")]
    inner: Box<dyn Connectable>,
}

impl CapabilityBound for Connector {
    fn debug_typename() -> &'static str {
        "Connector"
    }

    #[cfg(target_os = "fuchsia")]
    fn try_into_directory_entry(
        self: Arc<Self>,
        _scope: vfs::execution_scope::ExecutionScope,
        _token: Arc<crate::WeakInstanceToken>,
    ) -> Result<Arc<dyn vfs::directory::entry::DirectoryEntry>, crate::ConversionError> {
        Ok(vfs::service::endpoint(move |_scope, server_end| {
            let _ = self.send(server_end.into_zx_channel().into());
        }))
    }
}

impl Connector {
    #[cfg(target_os = "fuchsia")]
    pub fn new() -> (Receiver, Arc<Self>) {
        let (sender, receiver) = mpsc::unbounded();
        let receiver = Receiver::new(receiver);
        let this = Self::new_sendable(sender);
        (receiver, this)
    }

    pub fn new_sendable(connector: impl Connectable + 'static) -> Arc<Self> {
        #[cfg(target_os = "fuchsia")]
        {
            Arc::new(Self { inner: Box::new(connector) })
        }
        #[cfg(not(target_os = "fuchsia"))]
        {
            let _ = connector;
            Arc::new(Self {})
        }
    }

    #[cfg(target_os = "fuchsia")]
    pub fn send(&self, channel: zx::Channel) -> Result<(), ()> {
        self.inner.send(channel)
    }
}

#[cfg(target_os = "fuchsia")]
impl Connectable for Connector {
    fn send(&self, channel: zx::Channel) -> Result<(), ()> {
        self.send(channel)
    }
}

#[cfg(not(target_os = "fuchsia"))]
impl Connectable for Connector {}

#[cfg(all(test, target_os = "fuchsia"))]
mod tests {
    use super::*;
    use crate::Capability;
    use fidl::handle::{Channel, Rights};
    use fidl_fuchsia_component_sandbox as fsandbox;

    // NOTE: sending-and-receiving tests are written in `receiver.rs`.

    /// Tests that a Connector can be cloned by cloning its FIDL token.
    /// and capabilities sent to the original and clone arrive at the same Receiver.
    #[fuchsia::test]
    async fn fidl_clone() {
        let (receiver, sender) = Connector::new();

        // Send a channel through the Connector.
        let (ch1, _ch2) = Channel::create();
        sender.send(ch1).unwrap();

        // Convert the Sender to a FIDL token.
        let connector: fsandbox::Connector = sender.to_fsandbox();

        // Clone the Sender by cloning the token.
        let token_clone = fsandbox::Connector {
            token: connector.token.duplicate_handle(Rights::SAME_RIGHTS).unwrap(),
        };
        let connector_clone =
            match Capability::try_from(fsandbox::Capability::Connector(token_clone)).unwrap() {
                Capability::Connector(connector) => connector,
                capability @ _ => panic!("wrong type {capability:?}"),
            };

        // Send a channel through the cloned Sender.
        let (ch1, _ch2) = Channel::create();
        connector_clone.send(ch1).unwrap();

        // The Receiver should receive two channels, one from each connector.
        for _ in 0..2 {
            let _ch = receiver.receive().await.unwrap();
        }
    }
}
