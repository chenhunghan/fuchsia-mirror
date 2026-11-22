// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Module for IP level paths' maximum transmission unit (PMTU) size
//! cache support.

use core::num::NonZeroUsize;
use core::time::Duration;

use log::{debug, trace};
use lru::LruCache;
use net_types::ip::{GenericOverIp, Ip, IpAddress, IpVersionMarker, Ipv4Mtu, Ipv6Mtu, Mtu};
use netstack3_base::socket::SocketIpAddr;
use netstack3_base::{
    CoreTimerContext, HandleableTimer, IcmpIpExt, Icmpv4ErrorCode, Icmpv6ErrorCode, Instant,
    InstantBindingsTypes, IpDeviceAddr, TimerBindingsTypes, TimerContext,
};
use packet_formats::icmp::{IcmpDestUnreachable, Icmpv4DestUnreachableCode};

/// The source address of a path.
///
/// A path's source is an address of this node, so it must be a valid device
/// address: specified, non-multicast, and non-mapped.
pub type PmtuSrcAddr<A> = IpDeviceAddr<A>;

/// The destination address of a path.
///
/// A path's destination must be a valid socket address: specified and
/// non-mapped. Multicast destinations are permitted: per RFC 8201 section 3, a
/// multicast path's PMTU is the minimum PMTU across all the paths to the
/// members of the group.
pub type PmtuDstAddr<A> = SocketIpAddr<A>;

/// Time between PMTU maintenance operations.
///
/// Maintenance operations are things like resetting cached PMTU data to force
/// restart PMTU discovery to detect increases in a PMTU.
///
/// 1 hour.
// TODO(ghanan): Make this value configurable by runtime options.
const MAINTENANCE_PERIOD: Duration = Duration::from_secs(3600);

/// Time for a PMTU value to be considered stale.
///
/// 3 hours.
// TODO(ghanan): Make this value configurable by runtime options.
const PMTU_STALE_TIMEOUT: Duration = Duration::from_secs(10800);

const MAX_ENTRIES: NonZeroUsize = NonZeroUsize::new(256).unwrap();

/// The timer ID for the path MTU cache.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, Hash, GenericOverIp)]
#[generic_over_ip(I, Ip)]
pub struct PmtuTimerId<I: Ip>(IpVersionMarker<I>);

/// The core context for the path MTU cache.
pub trait PmtuContext<I: Ip, BT: PmtuBindingsTypes> {
    /// Calls a function with a mutable reference to the PMTU cache.
    fn with_state_mut<O, F: FnOnce(&mut PmtuCache<I, BT>) -> O>(&mut self, cb: F) -> O;
}

/// The bindings types for path MTU discovery.
pub trait PmtuBindingsTypes: TimerBindingsTypes + InstantBindingsTypes {}
impl<BT> PmtuBindingsTypes for BT where BT: TimerBindingsTypes + InstantBindingsTypes {}

/// The bindings execution context for path MTU discovery.
trait PmtuBindingsContext: PmtuBindingsTypes + TimerContext {}
impl<BC> PmtuBindingsContext for BC where BC: PmtuBindingsTypes + TimerContext {}

/// A handler for incoming PMTU events.
///
/// `PmtuHandler` serves as the interface between the transport layer and the IP
/// layer, which holds the PMTU cache. In production, method calls are delegated
/// to a real [`PmtuCache`], while in testing, method calls may be delegated to
/// a fake implementation.
pub trait PmtuHandler<I: Ip, BC> {
    /// Updates the PMTU between `src_ip` and `dst_ip` if `new_mtu` is less than
    /// the current PMTU.
    ///
    /// Returns the current PMTU after the update, whether or not it was updated.
    fn update_pmtu_if_less(
        &mut self,
        bindings_ctx: &mut BC,
        src_ip: PmtuSrcAddr<I::Addr>,
        dst_ip: PmtuDstAddr<I::Addr>,
        new_mtu: I::Mtu,
    ) -> I::Mtu;
}

fn maybe_schedule_timer<BC: PmtuBindingsContext>(
    bindings_ctx: &mut BC,
    timer: &mut BC::Timer,
    cache_is_empty: bool,
) {
    // Only attempt to create the next maintenance task if we still have
    // PMTU entries in the cache. If we don't, it would be a waste to
    // schedule the timer. We will let the next creation of a PMTU entry
    // create the timer.
    if cache_is_empty {
        return;
    }

    match bindings_ctx.scheduled_instant(timer) {
        Some(scheduled_at) => {
            let _: BC::Instant = scheduled_at;
            // Timer already set, nothing to do.
        }
        None => {
            // We only enter this match arm if a timer was not already set.
            assert_eq!(bindings_ctx.schedule_timer(MAINTENANCE_PERIOD, timer), None)
        }
    }
}

/// Enables a blanket implementation of [`PmtuHandler`].
///
/// Implementing this marker trait for a type enables a blanket implementation
/// of `PmtuHandler` given the other requirements are met.
pub trait UsePmtuHandlerBlanket {}

impl<I: Ip, BC: PmtuBindingsContext, CC: PmtuContext<I, BC> + UsePmtuHandlerBlanket>
    PmtuHandler<I, BC> for CC
{
    fn update_pmtu_if_less(
        &mut self,
        bindings_ctx: &mut BC,
        src_ip: PmtuSrcAddr<I::Addr>,
        dst_ip: PmtuDstAddr<I::Addr>,
        new_mtu: I::Mtu,
    ) -> I::Mtu {
        self.with_state_mut(|cache| {
            let now = bindings_ctx.now();
            let mtu = cache.update_pmtu_if_less(src_ip, dst_ip, new_mtu, now);
            let is_empty = cache.is_empty();
            maybe_schedule_timer(bindings_ctx, &mut cache.timer, is_empty);
            mtu
        })
    }
}

impl<I: Ip, BC: PmtuBindingsContext, CC: PmtuContext<I, BC>> HandleableTimer<CC, BC>
    for PmtuTimerId<I>
{
    fn handle(self, core_ctx: &mut CC, bindings_ctx: &mut BC, _: BC::UniqueTimerId) {
        let Self(IpVersionMarker { .. }) = self;
        core_ctx.with_state_mut(|cache| {
            let now = bindings_ctx.now();
            cache.handle_timer(now);
            let is_empty = cache.is_empty();
            maybe_schedule_timer(bindings_ctx, &mut cache.timer, is_empty);
        })
    }
}

/// The key used to identify a path.
///
/// This is a tuple of (src_ip, dst_ip) as a path is only identified by the
/// source and destination addresses.
// TODO(ghanan): Should device play a part in the key-ing of a path?
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct PmtuCacheKey<A: IpAddress>(PmtuSrcAddr<A>, PmtuDstAddr<A>);

impl<A: IpAddress> PmtuCacheKey<A> {
    fn new(src_ip: PmtuSrcAddr<A>, dst_ip: PmtuDstAddr<A>) -> Self {
        Self(src_ip, dst_ip)
    }
}

/// IP layer PMTU cache data.
#[derive(Debug, PartialEq)]
pub(crate) struct PmtuCacheData<M, I> {
    pmtu: M,
    last_updated: I,
}

impl<M, I: Instant> PmtuCacheData<M, I> {
    /// Construct a new `PmtuCacheData`.
    ///
    /// `last_updated` will be set to `now`.
    fn new(pmtu: M, now: I) -> Self {
        Self { pmtu, last_updated: now }
    }
}

/// A path MTU cache.
pub struct PmtuCache<I: Ip, BT: PmtuBindingsTypes> {
    cache: LruCache<PmtuCacheKey<I::Addr>, PmtuCacheData<I::Mtu, BT::Instant>>,
    timer: BT::Timer,
}

impl<I: Ip, BC: PmtuBindingsTypes + TimerContext> PmtuCache<I, BC> {
    pub(crate) fn new<CC: CoreTimerContext<PmtuTimerId<I>, BC>>(bindings_ctx: &mut BC) -> Self {
        Self {
            cache: LruCache::new(MAX_ENTRIES),
            timer: CC::new_timer(bindings_ctx, PmtuTimerId::default()),
        }
    }
}

impl<I: Ip, BT: PmtuBindingsTypes> PmtuCache<I, BT> {
    /// Gets the PMTU between `src_ip` and `dst_ip`.
    pub fn get_pmtu(
        &mut self,
        src_ip: PmtuSrcAddr<I::Addr>,
        dst_ip: PmtuDstAddr<I::Addr>,
    ) -> Option<I::Mtu> {
        self.cache.get_mut(&PmtuCacheKey::new(src_ip, dst_ip)).map(|x| x.pmtu)
    }

    /// Updates the PMTU between `src_ip` and `dst_ip` if `new_mtu` is less than
    /// the current PMTU.
    ///
    /// Returns the PMTU after updating the cache.
    fn update_pmtu_if_less(
        &mut self,
        src_ip: PmtuSrcAddr<I::Addr>,
        dst_ip: PmtuDstAddr<I::Addr>,
        new_mtu: I::Mtu,
        now: BT::Instant,
    ) -> I::Mtu {
        match self.get_pmtu(src_ip, dst_ip) {
            // A PMTU exists but it is less than or equal to `new_mtu` so no need to
            // update.
            Some(prev_mtu) if prev_mtu <= new_mtu => {
                trace!(
                    "update_pmtu_if_less: Not updating the PMTU between src {src_ip} and dst
                    {dst_ip} to {new_mtu:?}; is {prev_mtu:?}"
                );
                prev_mtu
            }
            // No PMTU exists, or it is greater than `new_mtu`, so update.
            None | Some(_) => {
                let _previous = self
                    .cache
                    .put(PmtuCacheKey::new(src_ip, dst_ip), PmtuCacheData::new(new_mtu, now));
                debug!("updated PMTU for path {src_ip} -> {dst_ip} to {new_mtu:?}");
                new_mtu
            }
        }
    }

    fn handle_timer(&mut self, now: BT::Instant) {
        // Make sure we expected this timer to fire.
        assert!(!self.cache.is_empty());

        // Remove all stale PMTU data to force restart the PMTU discovery
        // process. This will be ok because the next time we try to send a
        // packet to some node, we will update the PMTU with the first known
        // potential PMTU (the first link's (connected to the node attempting
        // PMTU discovery)) PMTU.
        //
        // TODO(ghanan): Add per-path options as per RFC 1981 section 5.3.
        //               Specifically, some links/paths may not need to have
        //               PMTU rediscovered as the PMTU will never change.
        //
        // TODO(ghanan): Consider not simply deleting all stale PMTU data as
        //               this may cause packets to be dropped every time the
        //               data seems to get stale when really it is still
        //               valid. Considering the use case, PMTU value changes
        //               may be infrequent so it may be enough to just use a
        //               long stale timer.
        //
        // TODO(https://fxbug.dev/404629697): once we actually use the PMTU
        // cache to inform IP fragmentation, consider discarding least-recently-
        // used entries rather than, or in addition to, entries that have been
        // in the cache for a long time.
        //
        self.cache
            .retain(|_k, v| now.saturating_duration_since(v.last_updated) < PMTU_STALE_TIMEOUT);
    }

    fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

/// Returns the PMTU reported by the ICMP error `err`.
///
/// Returns `None` if `err` is not a PMTU error (i.e. neither an ICMPv4
/// Destination Unreachable (Fragmentation Required) nor an ICMPv6 Packet Too
/// Big error), or if no valid PMTU can be determined from it.
pub fn pmtu_from_icmp_error<I: IcmpIpExt>(err: I::ErrorCode) -> Option<I::Mtu> {
    I::map_ip(
        err,
        |err| match err {
            Icmpv4ErrorCode::DestUnreachable {
                code: Icmpv4DestUnreachableCode::FragmentationRequired,
                message,
                original_packet_length,
            } => pmtu_from_icmpv4_fragmentation_required(&message, original_packet_length),
            Icmpv4ErrorCode::DestUnreachable { code: _, message: _, original_packet_length: _ }
            | Icmpv4ErrorCode::Redirect(_)
            | Icmpv4ErrorCode::TimeExceeded(_)
            | Icmpv4ErrorCode::ParameterProblem(_) => None,
        },
        |err| match err {
            Icmpv6ErrorCode::PacketTooBig(mtu) => Ipv6Mtu::new(mtu),
            Icmpv6ErrorCode::DestUnreachable(_)
            | Icmpv6ErrorCode::TimeExceeded(_)
            | Icmpv6ErrorCode::ParameterProblem(_) => None,
        },
    )
}

/// Returns the PMTU reported by an ICMPv4 Destination Unreachable
/// (Fragmentation Required) `message` for an original packet of total length
/// `original_packet_len`.
///
/// If `message` does not carry a Next-Hop MTU, the sender is assumed not to
/// implement RFC 1191 and the PMTU is estimated as the next lower plateau from
/// `original_packet_len`, per RFC 1191 section 5.
///
/// Returns `None` if the PMTU would be smaller than the IPv4 minimum link MTU.
pub(crate) fn pmtu_from_icmpv4_fragmentation_required(
    message: &IcmpDestUnreachable,
    original_packet_len: u16,
) -> Option<Ipv4Mtu> {
    let mtu = match message.next_hop_mtu() {
        Some(next_hop_mtu) => Mtu::new(u32::from(next_hop_mtu.get())),
        None => {
            debug!(
                "Next-Hop MTU is 0 so using the next lower PMTU plateau from {original_packet_len}"
            );
            next_lower_pmtu_plateau(Mtu::new(u32::from(original_packet_len)))?
        }
    };
    Ipv4Mtu::new(mtu)
}

/// Get next lower PMTU plateau value, if one exists.
fn next_lower_pmtu_plateau(start_mtu: Mtu) -> Option<Mtu> {
    /// Common MTU values taken from [RFC 1191 section 7.1].
    ///
    /// This list includes lower bounds of groups of common MTU values that are
    /// relatively close to each other, sorted in descending order.
    ///
    /// Note, the RFC does not actually include the value 1280 in the list of
    /// plateau values, but we include it here because it is the minimum IPv6
    /// MTU value and is not expected to be an uncommon value for MTUs.
    ///
    /// This list MUST be sorted in descending order; methods such as
    /// `next_lower_pmtu_plateau` assume `PMTU_PLATEAUS` has this property.
    ///
    /// We use this list when estimating PMTU values when doing PMTU discovery
    /// with IPv4 on paths with nodes that do not implement RFC 1191. This list
    /// is useful as in practice, relatively few MTU values are in use.
    ///
    /// [RFC 1191 section 7.1]: https://tools.ietf.org/html/rfc1191#section-7.1
    const PMTU_PLATEAUS: [Mtu; 12] = [
        Mtu::new(65535),
        Mtu::new(32000),
        Mtu::new(17914),
        Mtu::new(8166),
        Mtu::new(4352),
        Mtu::new(2002),
        Mtu::new(1492),
        Mtu::new(1280),
        Mtu::new(1006),
        Mtu::new(508),
        Mtu::new(296),
        Mtu::new(68),
    ];

    for i in 0..PMTU_PLATEAUS.len() {
        let pmtu = PMTU_PLATEAUS[i];

        if pmtu < start_mtu {
            // Current PMTU is less than `start_mtu` and we know `PMTU_PLATEAUS`
            // is sorted so this is the next best PMTU estimate.
            return Some(pmtu);
        }
    }

    None
}

#[cfg(any(test, feature = "testutils"))]
pub(crate) mod testutil {
    use core::num::NonZeroU16;

    use net_types::ip::Mtu;
    use netstack3_base::{IcmpIpExt, Icmpv4ErrorCode, Icmpv6ErrorCode};
    use packet_formats::icmp::{IcmpDestUnreachable, Icmpv4DestUnreachableCode};

    /// Constructs an ICMP PMTU error (ICMPv4 Destination Unreachable
    /// (Fragmentation Required) or ICMPv6 Packet Too Big) reporting `mtu` as it
    /// would appear on the wire.
    ///
    /// For IPv4, `mtu` is clamped to `u16::MAX` and the original packet length
    /// is left unset, so that a Next-Hop MTU of 0 does not resolve to a PMTU
    /// plateau.
    pub fn new_pmtu_error<I: IcmpIpExt>(mtu: Mtu) -> I::ErrorCode {
        I::map_ip_out(
            mtu,
            |mtu| {
                let next_hop_mtu = u16::try_from(mtu.get()).unwrap_or(u16::MAX);
                Icmpv4ErrorCode::DestUnreachable {
                    code: Icmpv4DestUnreachableCode::FragmentationRequired,
                    message: NonZeroU16::new(next_hop_mtu)
                        .map(IcmpDestUnreachable::new_for_frag_req)
                        .unwrap_or_default(),
                    original_packet_length: 0,
                }
            },
            |mtu| Icmpv6ErrorCode::PacketTooBig(mtu),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use core::num::NonZeroU16;

    use ip_test_macro::ip_test;
    use net_types::ip::{IpMtu as _, Ipv4, Ipv6};
    use net_types::{SpecifiedAddr, Witness as _};
    use netstack3_base::testutil::{
        FakeBindingsCtx, FakeInstant, FakeTimerCtxExt, TestIpExt, assert_empty,
    };
    use netstack3_base::{CtxPair, InstantContext, IntoCoreTimerCtx};
    use packet_formats::icmp::Icmpv6DestUnreachableCode;
    use test_case::test_case;

    struct FakePmtuContext<I: Ip> {
        cache: PmtuCache<I, FakeBindingsCtxImpl<I>>,
    }

    type FakeCtxImpl<I> = CtxPair<FakePmtuContext<I>, FakeBindingsCtxImpl<I>>;
    type FakeBindingsCtxImpl<I> = FakeBindingsCtx<PmtuTimerId<I>, (), (), ()>;

    impl<I: Ip> PmtuContext<I, FakeBindingsCtxImpl<I>> for FakePmtuContext<I> {
        fn with_state_mut<O, F: FnOnce(&mut PmtuCache<I, FakeBindingsCtxImpl<I>>) -> O>(
            &mut self,
            cb: F,
        ) -> O {
            cb(&mut self.cache)
        }
    }

    impl<I: Ip> UsePmtuHandlerBlanket for FakePmtuContext<I> {}

    fn new_context<I: Ip>() -> FakeCtxImpl<I> {
        FakeCtxImpl::with_default_bindings_ctx(|bindings_ctx| FakePmtuContext {
            cache: PmtuCache::new::<IntoCoreTimerCtx>(bindings_ctx),
        })
    }

    /// Get an IPv4 or IPv6 address within the same subnet as that of
    /// `TEST_ADDRS_*`, but with the last octet set to `3`.
    fn get_other_ip_address<I: TestIpExt>() -> SpecifiedAddr<I::Addr> {
        I::get_other_ip_address(3)
    }

    /// Returns `TEST_ADDRS`'s local address as a path source address.
    fn local_ip<I: TestIpExt>() -> PmtuSrcAddr<I::Addr> {
        PmtuSrcAddr::new(I::TEST_ADDRS.local_ip.get()).unwrap()
    }

    /// Returns `TEST_ADDRS`'s remote address as a path destination address.
    fn remote_ip<I: TestIpExt>() -> PmtuDstAddr<I::Addr> {
        PmtuDstAddr::new(I::TEST_ADDRS.remote_ip.get()).unwrap()
    }

    impl<I: Ip, BT: PmtuBindingsTypes> PmtuCache<I, BT> {
        /// Gets the last updated [`Instant`] when the PMTU between `src_ip` and
        /// `dst_ip` was updated.
        ///
        /// [`Instant`]: Instant
        fn get_last_updated(
            &mut self,
            src_ip: PmtuSrcAddr<I::Addr>,
            dst_ip: PmtuDstAddr<I::Addr>,
        ) -> Option<BT::Instant> {
            self.cache.get_mut(&PmtuCacheKey::new(src_ip, dst_ip)).map(|x| x.last_updated.clone())
        }
    }

    #[test_case(Mtu::new(65536) => Some(Mtu::new(65535)))]
    #[test_case(Mtu::new(65535) => Some(Mtu::new(32000)))]
    #[test_case(Mtu::new(65534) => Some(Mtu::new(32000)))]
    #[test_case(Mtu::new(32001) => Some(Mtu::new(32000)))]
    #[test_case(Mtu::new(32000) => Some(Mtu::new(17914)))]
    #[test_case(Mtu::new(31999) => Some(Mtu::new(17914)))]
    #[test_case(Mtu::new(1281)  => Some(Mtu::new(1280)))]
    #[test_case(Mtu::new(1280)  => Some(Mtu::new(1006)))]
    #[test_case(Mtu::new(69)    => Some(Mtu::new(68)))]
    #[test_case(Mtu::new(68)    => None)]
    #[test_case(Mtu::new(67)    => None)]
    #[test_case(Mtu::new(0)     => None)]
    fn test_next_lower_pmtu_plateau(start: Mtu) -> Option<Mtu> {
        next_lower_pmtu_plateau(start)
    }

    #[test_case(1500, 520 => Some(1500); "explicit mtu")]
    #[test_case(68, 520 => Some(68); "explicit min mtu")]
    #[test_case(67, 520 => None; "explicit below min mtu")]
    #[test_case(0, 520 => Some(508); "rfc1191 plateau")]
    #[test_case(0, 68 => None; "rfc1191 no lower plateau")]
    fn test_pmtu_from_icmpv4_fragmentation_required(
        next_hop_mtu: u16,
        orig_total_len: u16,
    ) -> Option<u16> {
        let message = NonZeroU16::new(next_hop_mtu)
            .map(IcmpDestUnreachable::new_for_frag_req)
            .unwrap_or_default();
        pmtu_from_icmpv4_fragmentation_required(&message, orig_total_len)
            .map(|mtu| u16::try_from(mtu.get().get()).unwrap())
    }

    #[test_case(Icmpv4ErrorCode::DestUnreachable {
            code: Icmpv4DestUnreachableCode::FragmentationRequired,
            message: IcmpDestUnreachable::new_for_frag_req(NonZeroU16::new(1500).unwrap()),
            original_packet_length: 1520,
        } => Ipv4Mtu::new(Mtu::new(1500)); "fragmentation required")]
    #[test_case(Icmpv4ErrorCode::DestUnreachable {
            code: Icmpv4DestUnreachableCode::FragmentationRequired,
            message: IcmpDestUnreachable::new_for_frag_req(NonZeroU16::new(67).unwrap()),
            original_packet_length: 1520,
        } => None; "fragmentation required below min mtu")]
    #[test_case(Icmpv4ErrorCode::DestUnreachable {
            code: Icmpv4DestUnreachableCode::DestHostUnreachable,
            message: IcmpDestUnreachable::default(),
            original_packet_length: 520,
        } => None; "dest host unreachable")]
    fn test_pmtu_from_icmpv4_error(err: Icmpv4ErrorCode) -> Option<Ipv4Mtu> {
        pmtu_from_icmp_error::<Ipv4>(err)
    }

    #[test_case(Icmpv6ErrorCode::PacketTooBig(Mtu::new(1500)) => Ipv6Mtu::new(Mtu::new(1500));
        "packet too big")]
    #[test_case(Icmpv6ErrorCode::PacketTooBig(Mtu::new(1279)) => None;
        "packet too big below min mtu")]
    #[test_case(Icmpv6ErrorCode::DestUnreachable(Icmpv6DestUnreachableCode::AddrUnreachable)
        => None; "dest unreachable")]
    fn test_pmtu_from_icmpv6_error(err: Icmpv6ErrorCode) -> Option<Ipv6Mtu> {
        pmtu_from_icmp_error::<Ipv6>(err)
    }

    fn get_pmtu<I: Ip>(
        core_ctx: &mut FakePmtuContext<I>,
        src_ip: PmtuSrcAddr<I::Addr>,
        dst_ip: PmtuDstAddr<I::Addr>,
    ) -> Option<I::Mtu> {
        core_ctx.cache.get_pmtu(src_ip, dst_ip)
    }

    fn get_last_updated<I: Ip>(
        core_ctx: &mut FakePmtuContext<I>,
        src_ip: PmtuSrcAddr<I::Addr>,
        dst_ip: PmtuDstAddr<I::Addr>,
    ) -> Option<FakeInstant> {
        core_ctx.cache.get_last_updated(src_ip, dst_ip)
    }

    #[ip_test(I)]
    fn test_ip_path_mtu_cache_ctx<I: TestIpExt>() {
        let local_ip = local_ip::<I>();
        let remote_ip = remote_ip::<I>();
        let FakeCtxImpl { mut core_ctx, mut bindings_ctx } = new_context::<I>();

        // Nothing in the cache yet
        assert_eq!(get_pmtu(&mut core_ctx, local_ip, remote_ip), None);
        assert_eq!(get_last_updated(&mut core_ctx, local_ip, remote_ip), None);

        let new_mtu1 = I::Mtu::new(Mtu::new(u32::from(I::MINIMUM_LINK_MTU.get()) + 50)).unwrap();
        let start_time = bindings_ctx.now();
        let duration = Duration::from_secs(1);

        // Advance time to 1s.
        assert_empty(bindings_ctx.trigger_timers_for(duration, &mut core_ctx));

        // Update pmtu from local to remote. PMTU should be updated to
        // `new_mtu1` and last updated instant should be updated to the start of
        // the test + 1s.
        assert_eq!(
            PmtuHandler::update_pmtu_if_less(
                &mut core_ctx,
                &mut bindings_ctx,
                local_ip,
                remote_ip,
                new_mtu1,
            ),
            new_mtu1
        );

        // Advance time to 2s.
        assert_empty(bindings_ctx.trigger_timers_for(duration, &mut core_ctx));

        // Make sure the update worked. PMTU should be updated to `new_mtu1` and
        // last updated instant should be updated to the start of the test + 1s
        // (when the update occurred.
        assert_eq!(get_pmtu(&mut core_ctx, local_ip, remote_ip).unwrap(), new_mtu1);
        assert_eq!(
            get_last_updated(&mut core_ctx, local_ip, remote_ip).unwrap(),
            start_time + duration
        );

        let new_mtu2 = I::Mtu::new(Mtu::new(u32::from(new_mtu1.get()) - 1)).unwrap();

        // Advance time to 3s.
        assert_empty(bindings_ctx.trigger_timers_for(duration, &mut core_ctx));

        // Updating again should return the last pmtu PMTU should be updated to
        // `new_mtu2` and last updated instant should be updated to the start of
        // the test + 3s.
        assert_eq!(
            PmtuHandler::update_pmtu_if_less(
                &mut core_ctx,
                &mut bindings_ctx,
                local_ip,
                remote_ip,
                new_mtu2,
            ),
            new_mtu2
        );

        // Advance time to 4s.
        assert_empty(bindings_ctx.trigger_timers_for(duration, &mut core_ctx));

        // Make sure the update worked. PMTU should be updated to `new_mtu2` and
        // last updated instant should be updated to the start of the test + 3s
        // (when the update occurred).
        assert_eq!(get_pmtu(&mut core_ctx, local_ip, remote_ip).unwrap(), new_mtu2);
        assert_eq!(
            get_last_updated(&mut core_ctx, local_ip, remote_ip).unwrap(),
            start_time + (duration * 3)
        );

        let new_mtu3 = I::Mtu::new(Mtu::new(u32::from(new_mtu2.get()) - 1)).unwrap();

        // Advance time to 5s.
        assert_empty(bindings_ctx.trigger_timers_for(duration, &mut core_ctx));

        // Make sure update only if new PMTU is less than current (it is). PMTU
        // should be updated to `new_mtu3` and last updated instant should be
        // updated to the start of the test + 5s.
        assert_eq!(
            PmtuHandler::update_pmtu_if_less(
                &mut core_ctx,
                &mut bindings_ctx,
                local_ip,
                remote_ip,
                new_mtu3,
            ),
            new_mtu3
        );

        // Advance time to 6s.
        assert_empty(bindings_ctx.trigger_timers_for(duration, &mut core_ctx));

        // Make sure the update worked. PMTU should be updated to `new_mtu3` and
        // last updated instant should be updated to the start of the test + 5s
        // (when the update occurred).
        assert_eq!(get_pmtu(&mut core_ctx, local_ip, remote_ip).unwrap(), new_mtu3);
        let last_updated = start_time + (duration * 5);
        assert_eq!(get_last_updated(&mut core_ctx, local_ip, remote_ip).unwrap(), last_updated);

        let new_mtu4 = I::Mtu::new(Mtu::new(u32::from(new_mtu3.get()) + 50)).unwrap();

        // Advance time to 7s.
        assert_empty(bindings_ctx.trigger_timers_for(duration, &mut core_ctx));

        // Make sure update only if new PMTU is less than current (it isn't)
        assert_eq!(
            PmtuHandler::update_pmtu_if_less(
                &mut core_ctx,
                &mut bindings_ctx,
                local_ip,
                remote_ip,
                new_mtu4,
            ),
            new_mtu3
        );

        // Advance time to 8s.
        assert_empty(bindings_ctx.trigger_timers_for(duration, &mut core_ctx));

        // Make sure the update didn't work. PMTU and last updated should not
        // have changed.
        assert_eq!(get_pmtu(&mut core_ctx, local_ip, remote_ip).unwrap(), new_mtu3);
        assert_eq!(get_last_updated(&mut core_ctx, local_ip, remote_ip).unwrap(), last_updated);

        let min_mtu = I::MINIMUM_LINK_MTU;

        // Advance time to 9s.
        assert_empty(bindings_ctx.trigger_timers_for(duration, &mut core_ctx));

        // Updating with the minimum link MTU should succeed.
        assert_eq!(
            PmtuHandler::update_pmtu_if_less(
                &mut core_ctx,
                &mut bindings_ctx,
                local_ip,
                remote_ip,
                min_mtu,
            ),
            min_mtu
        );

        // Advance time to 10s.
        assert_empty(bindings_ctx.trigger_timers_for(duration, &mut core_ctx));

        assert_eq!(get_pmtu(&mut core_ctx, local_ip, remote_ip).unwrap(), min_mtu);
        assert_eq!(
            get_last_updated(&mut core_ctx, local_ip, remote_ip).unwrap(),
            start_time + (duration * 9)
        );
    }

    #[ip_test(I)]
    fn test_ip_pmtu_task<I: TestIpExt>() {
        let local_ip = local_ip::<I>();
        let remote_ip = remote_ip::<I>();
        let FakeCtxImpl { mut core_ctx, mut bindings_ctx } = new_context::<I>();

        // Make sure there are no timers.
        bindings_ctx.timers.assert_no_timers_installed();

        let new_mtu1 = I::Mtu::new(Mtu::new(u32::from(I::MINIMUM_LINK_MTU.get()) + 50)).unwrap();
        let start_time = bindings_ctx.now();
        let duration = Duration::from_secs(1);

        // Advance time to 1s.
        assert_empty(bindings_ctx.trigger_timers_for(duration, &mut core_ctx));

        // Update pmtu from local to remote. PMTU should be updated to
        // `new_mtu1` and last updated instant should be updated to the start of
        // the test + 1s.
        assert_eq!(
            PmtuHandler::update_pmtu_if_less(
                &mut core_ctx,
                &mut bindings_ctx,
                local_ip,
                remote_ip,
                new_mtu1,
            ),
            new_mtu1
        );

        // Make sure a task got scheduled.
        bindings_ctx.timers.assert_timers_installed([(
            PmtuTimerId::default(),
            FakeInstant::from(MAINTENANCE_PERIOD + Duration::from_secs(1)),
        )]);

        // Advance time to 2s.
        assert_empty(bindings_ctx.trigger_timers_for(duration, &mut core_ctx));

        // Make sure the update worked. PMTU should be updated to `new_mtu1` and
        // last updated instant should be updated to the start of the test + 1s
        // (when the update occurred.
        assert_eq!(get_pmtu(&mut core_ctx, local_ip, remote_ip).unwrap(), new_mtu1);
        assert_eq!(
            get_last_updated(&mut core_ctx, local_ip, remote_ip).unwrap(),
            start_time + duration
        );

        // Advance time to 30mins.
        assert_empty(bindings_ctx.trigger_timers_for(duration * 1798, &mut core_ctx));

        // Update pmtu from local to another remote. PMTU should be updated to
        // `new_mtu1` and last updated instant should be updated to the start of
        // the test + 1s.
        let other_ip = PmtuDstAddr::try_from(get_other_ip_address::<I>()).unwrap();
        let new_mtu2 = I::Mtu::new(Mtu::new(u32::from(I::MINIMUM_LINK_MTU.get()) + 100)).unwrap();
        assert_eq!(
            PmtuHandler::update_pmtu_if_less(
                &mut core_ctx,
                &mut bindings_ctx,
                local_ip,
                other_ip,
                new_mtu2,
            ),
            new_mtu2
        );

        // Make sure there is still a task scheduled. (we know no timers got
        // triggered because the `run_for` methods returned 0 so far).
        bindings_ctx.timers.assert_timers_installed([(
            PmtuTimerId::default(),
            FakeInstant::from(MAINTENANCE_PERIOD + Duration::from_secs(1)),
        )]);

        // Make sure the update worked. PMTU should be updated to `new_mtu2` and
        // last updated instant should be updated to the start of the test +
        // 30mins + 2s (when the update occurred.
        assert_eq!(get_pmtu(&mut core_ctx, local_ip, other_ip).unwrap(), new_mtu2);
        assert_eq!(
            get_last_updated(&mut core_ctx, local_ip, other_ip).unwrap(),
            start_time + (duration * 1800)
        );
        // Make sure first update is still in the cache.
        assert_eq!(get_pmtu(&mut core_ctx, local_ip, remote_ip).unwrap(), new_mtu1);
        assert_eq!(
            get_last_updated(&mut core_ctx, local_ip, remote_ip).unwrap(),
            start_time + duration
        );

        // Advance time to 1hr + 1s. Should have triggered a timer.
        bindings_ctx.trigger_timers_for_and_expect(
            duration * 1801,
            [PmtuTimerId::default()],
            &mut core_ctx,
        );
        // Make sure none of the cache data has been marked as stale and
        // removed.
        assert_eq!(get_pmtu(&mut core_ctx, local_ip, remote_ip).unwrap(), new_mtu1);
        assert_eq!(
            get_last_updated(&mut core_ctx, local_ip, remote_ip).unwrap(),
            start_time + duration
        );
        assert_eq!(get_pmtu(&mut core_ctx, local_ip, other_ip).unwrap(), new_mtu2);
        assert_eq!(
            get_last_updated(&mut core_ctx, local_ip, other_ip).unwrap(),
            start_time + (duration * 1800)
        );
        // Should still have another task scheduled.
        bindings_ctx.timers.assert_timers_installed([(
            PmtuTimerId::default(),
            FakeInstant::from(MAINTENANCE_PERIOD * 2 + Duration::from_secs(1)),
        )]);

        // Advance time to 3hr + 1s. Should have triggered 2 timers.
        bindings_ctx.trigger_timers_for_and_expect(
            duration * 7200,
            [PmtuTimerId::default(), PmtuTimerId::default()],
            &mut core_ctx,
        );
        // Make sure only the earlier PMTU data got marked as stale and removed.
        assert_eq!(get_pmtu(&mut core_ctx, local_ip, remote_ip), None);
        assert_eq!(get_last_updated(&mut core_ctx, local_ip, remote_ip), None);
        assert_eq!(get_pmtu(&mut core_ctx, local_ip, other_ip).unwrap(), new_mtu2);
        assert_eq!(
            get_last_updated(&mut core_ctx, local_ip, other_ip).unwrap(),
            start_time + (duration * 1800)
        );
        // Should still have another task scheduled.
        bindings_ctx.timers.assert_timers_installed([(
            PmtuTimerId::default(),
            FakeInstant::from(MAINTENANCE_PERIOD * 4 + Duration::from_secs(1)),
        )]);

        // Advance time to 4hr + 1s. Should have triggered 1 timers.
        bindings_ctx.trigger_timers_for_and_expect(
            duration * 3600,
            [PmtuTimerId::default()],
            &mut core_ctx,
        );
        // Make sure both PMTU data got marked as stale and removed.
        assert_eq!(get_pmtu(&mut core_ctx, local_ip, remote_ip), None);
        assert_eq!(get_last_updated(&mut core_ctx, local_ip, remote_ip), None);
        assert_eq!(get_pmtu(&mut core_ctx, local_ip, other_ip), None);
        assert_eq!(get_last_updated(&mut core_ctx, local_ip, other_ip), None);
        // Should not have a task scheduled since there is no more PMTU data.
        bindings_ctx.timers.assert_no_timers_installed();
    }

    #[ip_test(I)]
    fn discard_lru<I: TestIpExt>() {
        let FakeCtxImpl { mut core_ctx, mut bindings_ctx } = new_context::<I>();
        let local_ip = local_ip::<I>();
        let remote_ip = PmtuSrcAddr::new(I::TEST_ADDRS.remote_ip.get()).unwrap();
        let other_ip = |i| PmtuDstAddr::try_from(I::get_other_ip_address(i)).unwrap();
        let max_mtu = I::Mtu::MAX;

        // Fill the cache to capacity.
        //
        // If this assertion trips because we've increased `MAX_ENTRIES`, we'll need to
        // update this test to use a different method than `get_other_ip_address` since
        // it only allows us to choose a single byte of the address.
        assert!(MAX_ENTRIES.get() <= usize::from(u8::MAX) + 1);
        for i in 0..MAX_ENTRIES.get() {
            let i = u8::try_from(i).unwrap();
            assert_eq!(
                PmtuHandler::update_pmtu_if_less(
                    &mut core_ctx,
                    &mut bindings_ctx,
                    local_ip,
                    other_ip(i),
                    max_mtu,
                ),
                max_mtu
            );
        }
        assert_eq!(core_ctx.cache.cache.len(), MAX_ENTRIES.get());

        // The next insertion should cause the LRU entry to be discarded.
        assert_eq!(
            PmtuHandler::update_pmtu_if_less(
                &mut core_ctx,
                &mut bindings_ctx,
                remote_ip,
                local_ip.into(),
                max_mtu,
            ),
            max_mtu
        );
        assert_eq!(core_ctx.cache.cache.len(), MAX_ENTRIES.get());
        assert_eq!(core_ctx.cache.get_pmtu(local_ip, other_ip(0)), None);
        assert_eq!(core_ctx.cache.get_pmtu(remote_ip, local_ip.into()), Some(max_mtu));
    }
}
