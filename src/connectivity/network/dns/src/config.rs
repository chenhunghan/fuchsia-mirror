// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_net as net;
use fidl_fuchsia_net_ext as net_ext;
use fuchsia_sync::Mutex;
use std::collections::HashSet;

/// Alias for a list of [`net::SocketAddress`].
///
/// The servers in the list are in priority order.
pub type ServerList = Vec<net::SocketAddress>;

/// Holds current [`ServerConfigState`] state.
#[derive(Debug, Default)]
struct ServerConfigInner {
    /// Servers set through `fuchsia.net.name/LookupAdmin`. Used while
    /// `policy_servers` is `None`.
    admin_servers: ServerList,
    /// DNS servers of the network this state is scoped to (for example the
    /// default network), from `fuchsia.net.policy.properties`. When set, they
    /// take precedence over `admin_servers`.
    policy_servers: Option<ServerList>,
    /// Socket marks of the network this state is scoped to.
    socket_marks: net::Marks,
}

/// The DNS configuration a resolver uses, shared between the tasks that
/// update it and the tasks that resolve names with it.
///
/// Each resolver owns its own [`ServerConfigState`], scoped to the one network
/// the resolver sends queries on (for example the default network). Policy
/// servers and socket marks therefore always describe that network and are
/// not keyed by network.
#[derive(Debug)]
pub struct ServerConfigState(Mutex<ServerConfigInner>);

/// The result of updating a [`ServerConfigState`].
#[derive(Debug, PartialEq, Eq)]
pub enum UpdateServersResult {
    /// Server list was updated to the provided value.
    Updated(ServerList),
    /// The effective server list (see [`ServerConfigState::servers`]) did not
    /// change. The stored `LookupAdmin` servers may still have changed while
    /// policy servers are in effect.
    NoChange,
    /// Invalid servers provided.
    InvalidServers,
}

fn validate_and_dedup(servers: &mut ServerList) -> bool {
    if servers.iter().any(|s| {
        // Addresses must not contain an unspecified or multicast address.
        let net_ext::SocketAddress(sockaddr) = From::from(*s);
        let ip = sockaddr.ip();
        ip.is_multicast() || ip.is_unspecified()
    }) {
        return false;
    }

    let mut set = HashSet::new();
    servers.retain(|s| set.insert(*s));
    true
}

impl ServerConfigState {
    /// Creates a new empty `ServerConfigState`.
    pub fn new() -> Self {
        Self(Mutex::new(ServerConfigInner::default()))
    }

    /// Returns the active servers.
    ///
    /// If per-network policy servers have been provided via
    /// [`Self::update_policy_servers`], they take precedence over servers set
    /// via [`Self::update_admin_servers`].
    pub fn servers(&self) -> ServerList {
        let Self(current) = self;
        let inner = current.lock();
        inner.policy_servers.as_ref().unwrap_or(&inner.admin_servers).clone()
    }

    /// Updates the fallback `LookupAdmin` server list after deduplication.
    pub fn update_admin_servers(&self, mut servers: ServerList) -> UpdateServersResult {
        let Self(current) = self;

        if !validate_and_dedup(&mut servers) {
            return UpdateServersResult::InvalidServers;
        }

        let mut inner = current.lock();
        if inner.admin_servers == servers {
            return UpdateServersResult::NoChange;
        }
        inner.admin_servers = servers.clone();
        if inner.policy_servers.is_some() {
            return UpdateServersResult::NoChange;
        }
        UpdateServersResult::Updated(servers)
    }

    /// Sets the DNS servers of the network this state is scoped to, from
    /// `fuchsia.net.policy.properties`, after deduplication.
    pub fn update_policy_servers(&self, mut servers: ServerList) -> UpdateServersResult {
        let Self(current) = self;

        if !validate_and_dedup(&mut servers) {
            return UpdateServersResult::InvalidServers;
        }

        let mut inner = current.lock();
        let prev_effective = inner.policy_servers.as_ref().unwrap_or(&inner.admin_servers);
        let changed = prev_effective != &servers;
        inner.policy_servers = Some(servers.clone());
        if changed { UpdateServersResult::Updated(servers) } else { UpdateServersResult::NoChange }
    }

    /// Clears any policy-watched servers so `LookupAdmin` servers take effect again.
    pub fn clear_policy_servers(&self) -> UpdateServersResult {
        let Self(current) = self;
        let mut inner = current.lock();
        let Some(prev_policy) = inner.policy_servers.take() else {
            return UpdateServersResult::NoChange;
        };
        if prev_policy == inner.admin_servers {
            UpdateServersResult::NoChange
        } else {
            UpdateServersResult::Updated(inner.admin_servers.clone())
        }
    }

    /// Returns the socket marks of the network this state is scoped to (for
    /// example the default network).
    pub fn socket_marks(&self) -> net::Marks {
        let Self(current) = self;
        current.lock().socket_marks.clone()
    }

    /// Sets the socket marks of the network this state is scoped to (for
    /// example when the default network changes).
    pub fn set_socket_marks(&self, marks: net::Marks) {
        let Self(current) = self;
        current.lock().socket_marks = marks;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;
    use net_declare::fidl_socket_addr;

    #[test]
    fn test_config_state() {
        let state = ServerConfigState::new();
        assert_eq!(state.servers(), []);

        // Ordering is respected.
        let values = [DHCP_SERVER, NDP_SERVER];
        assert_eq!(
            state.update_admin_servers(values.to_vec()),
            UpdateServersResult::Updated(values.to_vec())
        );
        assert_eq!(state.servers(), values);

        // Duplicates are removed.
        assert_eq!(
            state.update_admin_servers(vec![DHCP_SERVER, DHCP_SERVER, NDP_SERVER]),
            UpdateServersResult::NoChange
        );
        assert_eq!(state.servers(), values);

        // Bad addresses are rejected.
        for addr in [
            fidl_socket_addr!("0.0.0.0:1111"),
            fidl_socket_addr!("[::]:2222"),
            fidl_socket_addr!("224.0.0.1:3333"),
            fidl_socket_addr!("[ff02::1]:4444"),
        ] {
            assert_eq!(state.update_admin_servers(vec![addr]), UpdateServersResult::InvalidServers);
        }
        assert_eq!(state.servers(), values);

        // Empty inputs become empty output.
        assert_eq!(state.update_admin_servers(vec![]), UpdateServersResult::Updated(vec![]));
        assert_eq!(state.servers(), []);
    }

    #[test]
    fn test_policy_servers_override_admin_servers() {
        let state = ServerConfigState::new();

        let admin_values = vec![DHCP_SERVER, NDP_SERVER];
        assert_eq!(
            state.update_admin_servers(admin_values.clone()),
            UpdateServersResult::Updated(admin_values.clone())
        );
        assert_eq!(state.servers(), admin_values);

        // Policy servers override admin servers.
        let policy_values = vec![NDP_SERVER];
        assert_eq!(
            state.update_policy_servers(policy_values.clone()),
            UpdateServersResult::Updated(policy_values.clone())
        );
        assert_eq!(state.servers(), policy_values);

        // Updating admin servers while policy servers are active does not change effective servers.
        assert_eq!(state.update_admin_servers(vec![STATIC_SERVER]), UpdateServersResult::NoChange);
        assert_eq!(state.servers(), policy_values);

        // Setting the same policy servers again results in NoChange.
        assert_eq!(
            state.update_policy_servers(policy_values.clone()),
            UpdateServersResult::NoChange
        );

        // Duplicate policy servers are removed.
        assert_eq!(
            state.update_policy_servers(vec![NDP_SERVER, NDP_SERVER]),
            UpdateServersResult::NoChange
        );
        assert_eq!(state.servers(), policy_values);

        // Bad policy addresses are rejected and the previous policy servers are kept.
        for addr in [fidl_socket_addr!("0.0.0.0:1111"), fidl_socket_addr!("[ff02::1]:4444")] {
            assert_eq!(
                state.update_policy_servers(vec![DHCP_SERVER, addr]),
                UpdateServersResult::InvalidServers
            );
        }
        assert_eq!(state.servers(), policy_values);

        // Clearing policy servers restores admin servers when effective servers differ.
        assert_eq!(state.update_policy_servers(vec![]), UpdateServersResult::Updated(vec![]));
        assert_eq!(state.servers(), []);
        assert_eq!(state.clear_policy_servers(), UpdateServersResult::Updated(vec![STATIC_SERVER]));
        assert_eq!(state.servers(), [STATIC_SERVER]);
        // Clearing again when no policy servers are set returns NoChange.
        assert_eq!(state.clear_policy_servers(), UpdateServersResult::NoChange);
        // Clearing when policy servers equal admin servers returns NoChange.
        assert_eq!(state.update_policy_servers(vec![STATIC_SERVER]), UpdateServersResult::NoChange);
        assert_eq!(state.clear_policy_servers(), UpdateServersResult::NoChange);
    }

    #[test]
    fn test_socket_marks() {
        let state = ServerConfigState::new();
        assert_eq!(state.socket_marks(), net::Marks::default());

        assert_eq!(
            state.update_policy_servers(vec![NDP_SERVER]),
            UpdateServersResult::Updated(vec![NDP_SERVER])
        );

        let marks = net::Marks { mark_1: Some(42), mark_2: Some(99), ..Default::default() };
        state.set_socket_marks(marks.clone());
        assert_eq!(state.socket_marks(), marks);

        let updated_marks = net::Marks { mark_1: Some(43), mark_2: Some(99), ..Default::default() };
        state.set_socket_marks(updated_marks.clone());
        assert_eq!(state.socket_marks(), updated_marks);
        assert_eq!(state.servers(), [NDP_SERVER]);
    }
}
