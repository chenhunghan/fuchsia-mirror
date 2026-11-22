// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! DNS Server watcher.

mod stream;
#[cfg(test)]
mod test_util;

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};

use fidl_fuchsia_net::SocketAddress;
use fidl_fuchsia_net_name::{
    DelegatedDnsServerSource, DhcpDnsServerSource, Dhcpv6DnsServerSource, DnsServer_,
    DnsServerSource, NdpDnsServerSource, StaticDnsServerSource,
};

pub use self::stream::*;

/// The default DNS server port.
pub const DEFAULT_DNS_PORT: u16 = 53;

/// The scope of a [`DnsServer_`] used when deduplicating [`DnsServers::consolidated_dns_servers`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DnsServerScope {
    Global,
    FuchsiaInterface(u64),
    Delegated(u64),
}

impl From<Option<&DnsServerSource>> for DnsServerScope {
    fn from(source: Option<&DnsServerSource>) -> Self {
        match source {
            Some(
                DnsServerSource::Dhcp(DhcpDnsServerSource { source_interface: Some(id), .. })
                | DnsServerSource::Ndp(NdpDnsServerSource { source_interface: Some(id), .. })
                | DnsServerSource::Dhcpv6(Dhcpv6DnsServerSource {
                    source_interface: Some(id), ..
                }),
            ) => Self::FuchsiaInterface(*id),
            Some(DnsServerSource::Delegated(DelegatedDnsServerSource {
                source_interface: Some(id),
                ..
            })) => Self::Delegated(*id),
            Some(
                DnsServerSource::StaticSource(_)
                | DnsServerSource::Dhcp(DhcpDnsServerSource { source_interface: None, .. })
                | DnsServerSource::Ndp(NdpDnsServerSource { source_interface: None, .. })
                | DnsServerSource::Dhcpv6(Dhcpv6DnsServerSource { source_interface: None, .. })
                | DnsServerSource::Delegated(DelegatedDnsServerSource {
                    source_interface: None, ..
                })
                | DnsServerSource::__SourceBreaking { .. },
            )
            | None => Self::Global,
        }
    }
}

/// The DNS servers learned from all sources.
#[derive(Debug, Default)]
pub struct DnsServers {
    /// DNS servers obtained from some default configurations.
    ///
    /// These servers will have the lowest priority of all servers.
    default: Vec<DnsServer_>,

    /// DNS servers obtained from DHCPv4 clients, keyed by interface ID.
    dhcpv4: BTreeMap<u64, Vec<DnsServer_>>,

    /// DNS servers obtained from DHCPv6 clients, keyed by interface ID.
    dhcpv6: BTreeMap<u64, Vec<DnsServer_>>,

    /// DNS servers obtained from NDP clients, keyed by interface ID.
    ndp: BTreeMap<u64, Vec<DnsServer_>>,

    /// DNS servers obtained from delegated networks.
    delegated: Vec<DnsServer_>,
}

impl DnsServers {
    /// Sets the DNS servers discovered from `source`.
    // TODO(https://fxbug.dev/42133326): Make sure `servers` only contain servers that could be obtained
    // from `source`.
    pub fn set_servers_from_source(
        &mut self,
        source: DnsServersUpdateSource,
        servers: Vec<DnsServer_>,
    ) {
        let Self { default, dhcpv4, dhcpv6, ndp, delegated } = self;

        match source {
            DnsServersUpdateSource::Default => *default = servers,
            DnsServersUpdateSource::Dhcpv4 { interface_id } => {
                // We discard existing servers since they are being replaced with
                // `servers` - the old servers are useless to us now.
                let _: Option<Vec<DnsServer_>> = if servers.is_empty() {
                    dhcpv4.remove(&interface_id)
                } else {
                    dhcpv4.insert(interface_id, servers)
                };
            }
            DnsServersUpdateSource::Dhcpv6 { interface_id } => {
                // We discard existing servers since they are being replaced with
                // `servers` - the old servers are useless to us now.
                let _: Option<Vec<DnsServer_>> = if servers.is_empty() {
                    dhcpv6.remove(&interface_id)
                } else {
                    dhcpv6.insert(interface_id, servers)
                };
            }
            DnsServersUpdateSource::Ndp { interface_id } => {
                // We discard existing servers since they are being replaced with
                // `servers` - the old servers are useless to us now.
                let _: Option<Vec<DnsServer_>> = if servers.is_empty() {
                    ndp.remove(&interface_id)
                } else {
                    ndp.insert(interface_id, servers)
                };
            }
            DnsServersUpdateSource::Delegated => *delegated = servers,
        }
    }

    /// Returns a consolidated list of server addresses.
    ///
    /// The servers will be returned deduplicated by their address and sorted by the source
    /// that each server was learned from. The servers will be sorted in most to least
    /// preferred order, with the most preferred server first. The preference of the servers
    /// is NDP, DHCPv4, DHCPv6 then Static, where NDP is the most preferred. Ordering across
    /// sources of the same source-kind is sorted by interface ID (lowest first), and ordering
    /// within a source is maintained.
    ///
    /// Note, if multiple `DnsServer_`s have the same address but different sources, only
    /// the `DnsServer_` with the most preferred source will be present in the consolidated
    /// list of servers.
    // TODO(https://fxbug.dev/42133571): Consider ordering across sources of the same source-kind based on some
    // metric.
    pub fn consolidated(&self) -> Vec<SocketAddress> {
        self.consolidate_filter_map(|x| x.address)
    }

    fn sorted_servers(&self) -> Vec<DnsServer_> {
        let Self { default, dhcpv4, dhcpv6, ndp, delegated } = self;
        let mut servers = delegated
            .iter()
            .chain(dhcpv4.values().flatten())
            .chain(ndp.values().flatten())
            .chain(dhcpv6.values().flatten())
            .cloned()
            .collect::<Vec<_>>();
        // Sorting happens before deduplication to ensure that when multiple sources report the same
        // address, the highest priority source wins.
        //
        // `sort_by` maintains the order of equal elements. This is required to maintain ordering
        // within a source of DNS servers. `sort_unstable_by` may not preserve this ordering.
        servers.sort_by(Self::ordering);
        // Default servers are considered to have the lowest priority so we append them to the end
        // of the list of sorted dynamically learned servers.
        servers.extend(default.clone());
        servers
    }

    /// Returns a consolidated list of [`DnsServer_`] structs.
    ///
    /// The servers will be returned deduplicated per source scope (per Fuchsia interface,
    /// per delegated network, or global/static) by their address and sorted by the source
    /// that each server was learned from. The servers will be sorted in most to least
    /// preferred order, with the most preferred server first. See doc comment on Self::ordering
    /// for order of preference based on ServerSource.
    ///
    /// Preserving entries across distinct interfaces and static sources ensures that per-network
    /// DNS server lists can be accurately reconstructed even when multiple networks configure
    /// the same DNS server IP address.
    // TODO(https://fxbug.dev/42133571): Consider ordering across sources of the
    // same source-kind based on some metric.
    pub fn consolidated_dns_servers(&self) -> Vec<DnsServer_> {
        let mut servers = self.sorted_servers();
        let mut seen = HashSet::new();
        servers.retain(move |s| seen.insert((DnsServerScope::from(s.source.as_ref()), s.address)));
        servers
    }

    /// Returns a consolidated list of servers, with the mapping function `f` applied.
    ///
    /// See `consolidated` for details on ordering.
    fn consolidate_filter_map<T, F: Fn(DnsServer_) -> Option<T>>(&self, f: F) -> Vec<T> {
        let mut servers = self.sorted_servers();
        let mut addresses = HashSet::new();
        servers.retain(move |s| addresses.insert(s.address));
        servers.into_iter().filter_map(f).collect()
    }

    /// Returns the ordering of [`DnsServer_`]s.
    ///
    /// The ordering from most to least preferred is is Delegated, DHCP, NDP, DHCPv6, then
    /// Static. Preference is currently informed by a goal of keeping servers discovered via the
    /// same internet protocol together and preferring network-supplied configurations over
    /// product-supplied ones. Delegated DNS servers are provided when the current default
    /// network was provisioned by an agent other than Fuchsia.
    ///
    /// TODO(https://fxbug.dev/42125772): We currently prioritize IPv4 servers over IPv6 as our
    /// DHCPv6 client does not yet support stateful address assignment. This can result in
    /// situations where we can discover v6 nameservers that can't be reached as we've not
    /// discovered a global address.
    ///
    /// An unspecified source will be treated as a static address.
    fn ordering(a: &DnsServer_, b: &DnsServer_) -> Ordering {
        let ordering = |source| match source {
            Some(&DnsServerSource::Delegated(DelegatedDnsServerSource {
                source_interface: _,
                ..
            })) => 0,
            Some(&DnsServerSource::Dhcp(DhcpDnsServerSource { source_interface: _, .. })) => 1,
            Some(&DnsServerSource::Ndp(NdpDnsServerSource { source_interface: _, .. })) => 2,
            Some(&DnsServerSource::Dhcpv6(Dhcpv6DnsServerSource {
                source_interface: _, ..
            })) => 3,
            Some(&DnsServerSource::StaticSource(StaticDnsServerSource { .. })) => 4,
            Some(&DnsServerSource::__SourceBreaking { .. }) | None => 5,
        };
        let a = ordering(a.source.as_ref());
        let b = ordering(b.source.as_ref());
        std::cmp::Ord::cmp(&a, &b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::constants::*;

    #[test]
    fn deduplicate_within_source() {
        // Simple deduplication and sorting of repeated `DnsServer_`.
        let servers = DnsServers {
            default: vec![ndp_server(), static_server(), ndp_server(), static_server()],
            // `DHCPV4/6_SERVER2` would normally only come from an interface with ID
            // `DHCPV4/6_SERVER2_INTERFACE_ID`, but we are just testing deduplication
            // logic here.
            dhcpv4: [
                (DHCPV4_SERVER1_INTERFACE_ID, vec![dhcpv4_server1(), dhcpv4_server2()]),
                (DHCPV4_SERVER2_INTERFACE_ID, vec![dhcpv4_server1(), dhcpv4_server2()]),
            ]
            .into_iter()
            .collect(),
            dhcpv6: [
                (DHCPV6_SERVER1_INTERFACE_ID, vec![dhcpv6_server1(), dhcpv6_server2()]),
                (DHCPV6_SERVER2_INTERFACE_ID, vec![dhcpv6_server1(), dhcpv6_server2()]),
            ]
            .into_iter()
            .collect(),
            ndp: [(NDP_SERVER_INTERFACE_ID, vec![ndp_server(), ndp_server()])]
                .into_iter()
                .collect(),
            delegated: vec![delegated_server1(), delegated_server2(), delegated_server1()],
        };
        // Ordering across sources of the same source-kind is deterministic (sorted by
        // `BTreeMap` interface ID ascending), and both DHCPv6 sources have the same servers in
        // the same order. With deduplication, only the lowest-ID source's servers remain.
        assert_eq!(
            servers.consolidated(),
            vec![
                DELEGATED_SOURCE_SOCKADDR1,
                DELEGATED_SOURCE_SOCKADDR2,
                DHCPV4_SOURCE_SOCKADDR1,
                DHCPV4_SOURCE_SOCKADDR2,
                NDP_SOURCE_SOCKADDR,
                DHCPV6_SOURCE_SOCKADDR1,
                DHCPV6_SOURCE_SOCKADDR2,
                STATIC_SOURCE_SOCKADDR,
            ],
        );
    }

    #[test]
    fn default_low_prio() {
        // Default servers should always have low priority, but if the same server
        // is observed by a higher priority source, then use the higher source for
        // ordering.
        let servers = DnsServers {
            default: vec![static_server(), dhcpv4_server1(), dhcpv6_server1(), delegated_server1()],
            dhcpv4: [
                (DHCPV4_SERVER1_INTERFACE_ID, vec![dhcpv4_server1()]),
                (DHCPV4_SERVER2_INTERFACE_ID, vec![dhcpv4_server1()]),
            ]
            .into_iter()
            .collect(),
            dhcpv6: [
                (DHCPV6_SERVER1_INTERFACE_ID, vec![dhcpv6_server1()]),
                (DHCPV6_SERVER2_INTERFACE_ID, vec![dhcpv6_server2()]),
            ]
            .into_iter()
            .collect(),
            ndp: [(NDP_SERVER_INTERFACE_ID, vec![ndp_server()])].into_iter().collect(),
            delegated: vec![delegated_server1()],
        };
        // Servers from different interfaces of the same source-kind are sorted by `BTreeMap`
        // interface ID ascending.
        let mut got = servers.consolidated();
        let mut got = got.drain(..);
        let want_delegated = [DELEGATED_SOURCE_SOCKADDR1];
        assert_eq!(
            HashSet::from_iter(got.by_ref().take(want_delegated.len())),
            HashSet::from(want_delegated),
        );

        let want_dhcpv4 = [DHCPV4_SOURCE_SOCKADDR1];
        assert_eq!(
            HashSet::from_iter(got.by_ref().take(want_dhcpv4.len())),
            HashSet::from(want_dhcpv4),
        );

        let want_ndp = [NDP_SOURCE_SOCKADDR];
        assert_eq!(HashSet::from_iter(got.by_ref().take(want_ndp.len())), HashSet::from(want_ndp));

        let want_dhcpv6 = [DHCPV6_SOURCE_SOCKADDR1, DHCPV6_SOURCE_SOCKADDR2];
        assert_eq!(
            HashSet::from_iter(got.by_ref().take(want_dhcpv6.len())),
            HashSet::from(want_dhcpv6),
        );

        let want_rest = [STATIC_SOURCE_SOCKADDR];
        assert_eq!(got.as_slice(), want_rest);
    }

    #[test]
    fn deduplicate_across_sources() {
        // Deduplication and sorting of same address across different sources.

        // DHCPv6 is not as preferred as NDP so this should not be in the consolidated
        // servers list.
        let dhcpv6_with_ndp_address = || DnsServer_ {
            address: Some(NDP_SOURCE_SOCKADDR),
            source: Some(DnsServerSource::Dhcpv6(Dhcpv6DnsServerSource {
                source_interface: Some(DHCPV6_SERVER1_INTERFACE_ID),
                ..Default::default()
            })),
            ..Default::default()
        };
        let mut dhcpv6 = BTreeMap::new();
        assert_matches::assert_matches!(
            dhcpv6.insert(
                DHCPV6_SERVER1_INTERFACE_ID,
                vec![dhcpv6_with_ndp_address(), dhcpv6_server1()]
            ),
            None
        );
        let servers = DnsServers {
            default: vec![static_server()],
            dhcpv4: [(DHCPV4_SERVER1_INTERFACE_ID, vec![dhcpv4_server1()])].into_iter().collect(),
            dhcpv6: [(
                DHCPV6_SERVER1_INTERFACE_ID,
                vec![dhcpv6_with_ndp_address(), dhcpv6_server1()],
            )]
            .into_iter()
            .collect(),
            ndp: [(NDP_SERVER_INTERFACE_ID, vec![ndp_server(), dhcpv6_with_ndp_address()])]
                .into_iter()
                .collect(),
            delegated: vec![],
        };
        let expected_servers =
            vec![dhcpv4_server1(), ndp_server(), dhcpv6_server1(), static_server()];
        assert_eq!(servers.consolidate_filter_map(Some), expected_servers);
        let expected_sockaddrs = vec![
            DHCPV4_SOURCE_SOCKADDR1,
            NDP_SOURCE_SOCKADDR,
            DHCPV6_SOURCE_SOCKADDR1,
            STATIC_SOURCE_SOCKADDR,
        ];
        assert_eq!(servers.consolidated(), expected_sockaddrs);

        // NDP is more preferred than DHCPv6 so `dhcpv6_server1()` should not be in the
        // consolidated list of servers.
        let ndp_with_dhcpv6_sockaddr1 = || DnsServer_ {
            address: Some(DHCPV6_SOURCE_SOCKADDR1),
            source: Some(DnsServerSource::Ndp(NdpDnsServerSource {
                source_interface: Some(NDP_SERVER_INTERFACE_ID),
                ..Default::default()
            })),
            ..Default::default()
        };

        let mut dhcpv6 = BTreeMap::new();
        assert_matches::assert_matches!(
            dhcpv6.insert(DHCPV6_SERVER1_INTERFACE_ID, vec![dhcpv6_server1()]),
            None
        );
        assert_matches::assert_matches!(
            dhcpv6.insert(DHCPV6_SERVER2_INTERFACE_ID, vec![dhcpv6_server2()]),
            None
        );
        let servers = DnsServers {
            default: vec![static_server()],
            dhcpv4: Default::default(),
            dhcpv6,
            ndp: [(NDP_SERVER_INTERFACE_ID, vec![ndp_with_dhcpv6_sockaddr1()])]
                .into_iter()
                .collect(),
            delegated: vec![],
        };
        let expected_servers = vec![ndp_with_dhcpv6_sockaddr1(), dhcpv6_server2(), static_server()];
        assert_eq!(servers.consolidate_filter_map(Some), expected_servers);
        let expected_sockaddrs =
            vec![DHCPV6_SOURCE_SOCKADDR1, DHCPV6_SOURCE_SOCKADDR2, STATIC_SOURCE_SOCKADDR];
        assert_eq!(servers.consolidated(), expected_sockaddrs);
    }

    #[test]
    fn consolidated_dns_servers_preserves_across_interfaces_and_static() {
        let dhcpv4_iface2_same_addr = || DnsServer_ {
            address: Some(DHCPV4_SOURCE_SOCKADDR1),
            source: Some(DnsServerSource::Dhcp(DhcpDnsServerSource {
                source_interface: Some(DHCPV4_SERVER2_INTERFACE_ID),
                ..Default::default()
            })),
            ..Default::default()
        };
        let static_same_addr = || DnsServer_ {
            address: Some(DHCPV4_SOURCE_SOCKADDR1),
            source: Some(DnsServerSource::StaticSource(StaticDnsServerSource::default())),
            ..Default::default()
        };
        // On the same interface (NDP_SERVER_INTERFACE_ID), NDP is preferred over DHCPv6.
        let dhcpv6_same_iface_as_ndp = || DnsServer_ {
            address: Some(NDP_SOURCE_SOCKADDR),
            source: Some(DnsServerSource::Dhcpv6(Dhcpv6DnsServerSource {
                source_interface: Some(NDP_SERVER_INTERFACE_ID),
                ..Default::default()
            })),
            ..Default::default()
        };

        let servers = DnsServers {
            default: vec![static_same_addr(), static_server()],
            dhcpv4: [
                (DHCPV4_SERVER1_INTERFACE_ID, vec![dhcpv4_server1()]),
                (DHCPV4_SERVER2_INTERFACE_ID, vec![dhcpv4_iface2_same_addr()]),
            ]
            .into_iter()
            .collect(),
            dhcpv6: [(NDP_SERVER_INTERFACE_ID, vec![dhcpv6_same_iface_as_ndp()])]
                .into_iter()
                .collect(),
            ndp: [(NDP_SERVER_INTERFACE_ID, vec![ndp_server()])].into_iter().collect(),
            delegated: vec![],
        };

        // `consolidated_dns_servers` keeps identical addresses across distinct interfaces and
        // static sources, while deduplicating within the same interface (keeping NDP over DHCPv6).
        assert_eq!(
            servers.consolidated_dns_servers(),
            vec![
                dhcpv4_server1(),
                dhcpv4_iface2_same_addr(),
                ndp_server(),
                static_same_addr(),
                static_server(),
            ]
        );

        // `consolidated` still deduplicates globally by address.
        assert_eq!(
            servers.consolidated(),
            vec![DHCPV4_SOURCE_SOCKADDR1, NDP_SOURCE_SOCKADDR, STATIC_SOURCE_SOCKADDR]
        );
    }

    #[test]
    fn test_dns_servers_ordering() {
        assert_eq!(
            DnsServers::ordering(&delegated_server1(), &delegated_server1()),
            Ordering::Equal
        );
        assert_eq!(DnsServers::ordering(&ndp_server(), &ndp_server()), Ordering::Equal);
        assert_eq!(DnsServers::ordering(&dhcpv4_server1(), &dhcpv4_server1()), Ordering::Equal);
        assert_eq!(DnsServers::ordering(&dhcpv6_server1(), &dhcpv6_server1()), Ordering::Equal);
        assert_eq!(DnsServers::ordering(&static_server(), &static_server()), Ordering::Equal);
        assert_eq!(
            DnsServers::ordering(&unspecified_source_server(), &unspecified_source_server()),
            Ordering::Equal
        );

        let mut servers = vec![
            unspecified_source_server(),
            dhcpv6_server1(),
            dhcpv4_server1(),
            static_server(),
            ndp_server(),
            delegated_server1(),
        ];
        servers.sort_by(DnsServers::ordering);
        assert_eq!(
            servers,
            vec![
                delegated_server1(),
                dhcpv4_server1(),
                ndp_server(),
                dhcpv6_server1(),
                static_server(),
                unspecified_source_server(),
            ]
        );
    }

    #[test]
    fn consolidated_dns_servers_delegated_scoping() {
        let shared_addr = DHCPV4_SOURCE_SOCKADDR1;
        let shared_interface_id = DHCPV4_SERVER1_INTERFACE_ID;
        let delegated_same_iface_id = || DnsServer_ {
            address: Some(shared_addr),
            source: Some(DnsServerSource::Delegated(DelegatedDnsServerSource {
                source_interface: Some(shared_interface_id),
                ..Default::default()
            })),
            ..Default::default()
        };
        let delegated_other_iface_id = || DnsServer_ {
            address: Some(shared_addr),
            source: Some(DnsServerSource::Delegated(DelegatedDnsServerSource {
                source_interface: Some(DELEGATED_SERVER2_INTERFACE_ID),
                ..Default::default()
            })),
            ..Default::default()
        };
        let delegated_no_iface = || DnsServer_ {
            address: Some(STATIC_SOURCE_SOCKADDR),
            source: Some(DnsServerSource::Delegated(DelegatedDnsServerSource {
                source_interface: None,
                ..Default::default()
            })),
            ..Default::default()
        };

        let servers = DnsServers {
            // `static_server()` shares `STATIC_SOURCE_SOCKADDR` with `delegated_no_iface()`, and
            // both fall into `DnsServerScope::Global`. Because Delegated has higher ordering
            // priority than StaticSource, `delegated_no_iface()` must win and deduplicate
            // `static_server()`.
            default: vec![static_server()],
            dhcpv4: [(shared_interface_id, vec![dhcpv4_server1()])].into_iter().collect(),
            dhcpv6: Default::default(),
            ndp: Default::default(),
            delegated: vec![
                delegated_same_iface_id(),
                delegated_other_iface_id(),
                delegated_no_iface(),
            ],
        };

        // `Delegated(Some(id))` is scoped separately from `FuchsiaInterface(id)` and from other
        // `Delegated(Some(id2))`, whereas `Delegated(None)` shares `Global` scope with Static.
        assert_eq!(
            servers.consolidated_dns_servers(),
            vec![
                delegated_same_iface_id(),
                delegated_other_iface_id(),
                delegated_no_iface(),
                dhcpv4_server1(),
            ]
        );
        assert_eq!(servers.consolidated(), vec![shared_addr, STATIC_SOURCE_SOCKADDR]);
    }

    #[test]
    fn consolidated_orders_distinct_interfaces_by_id() {
        let ndp_server_higher_id = DnsServer_ {
            address: Some(net_declare::fidl_socket_addr!("[2001:db8::9999]:53")),
            source: Some(DnsServerSource::Ndp(NdpDnsServerSource {
                source_interface: Some(NDP_SERVER_INTERFACE_ID + 10),
                ..Default::default()
            })),
            ..Default::default()
        };

        // Insert higher interface IDs before lower IDs; BTreeMap must yield lowest interface ID
        // first for each source-kind.
        let servers = DnsServers {
            default: vec![],
            dhcpv4: [
                (DHCPV4_SERVER2_INTERFACE_ID, vec![dhcpv4_server2()]),
                (DHCPV4_SERVER1_INTERFACE_ID, vec![dhcpv4_server1()]),
            ]
            .into_iter()
            .collect(),
            dhcpv6: [
                (DHCPV6_SERVER2_INTERFACE_ID, vec![dhcpv6_server2()]),
                (DHCPV6_SERVER1_INTERFACE_ID, vec![dhcpv6_server1()]),
            ]
            .into_iter()
            .collect(),
            ndp: [
                (NDP_SERVER_INTERFACE_ID + 10, vec![ndp_server_higher_id.clone()]),
                (NDP_SERVER_INTERFACE_ID, vec![ndp_server()]),
            ]
            .into_iter()
            .collect(),
            delegated: vec![],
        };

        assert_eq!(
            servers.consolidated_dns_servers(),
            vec![
                dhcpv4_server1(),
                dhcpv4_server2(),
                ndp_server(),
                ndp_server_higher_id,
                dhcpv6_server1(),
                dhcpv6_server2(),
            ]
        );
        assert_eq!(
            servers.consolidated(),
            vec![
                DHCPV4_SOURCE_SOCKADDR1,
                DHCPV4_SOURCE_SOCKADDR2,
                NDP_SOURCE_SOCKADDR,
                net_declare::fidl_socket_addr!("[2001:db8::9999]:53"),
                DHCPV6_SOURCE_SOCKADDR1,
                DHCPV6_SOURCE_SOCKADDR2,
            ]
        );
    }
}
