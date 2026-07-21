use core::result::Result;
use heapless::{LinearMap, Vec};

use super::{Interface, InterfaceInner};
#[cfg(any(feature = "proto-ipv4", feature = "proto-ipv6"))]
use super::{IpPayload, Packet, check};
use crate::config::{IFACE_MAX_ADDR_COUNT, IFACE_MAX_MULTICAST_GROUP_COUNT};
use crate::phy::{Device, PacketMeta};
use crate::wire::*;

/// Error type for `join_multicast_group`, `leave_multicast_group`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum MulticastError {
    /// The table of joined multicast groups is already full.
    GroupTableFull,
    /// Cannot join/leave the given multicast group.
    Unaddressable,
}

#[cfg(feature = "proto-ipv4")]
pub(crate) enum IgmpReportState {
    Inactive,
    ToGeneralQuery {
        version: IgmpVersion,
        timeout: crate::time::Instant,
        interval: crate::time::Duration,
        next_index: usize,
    },
    ToSpecificQuery {
        version: IgmpVersion,
        timeout: crate::time::Instant,
        group: Ipv4Address,
    },
}

#[cfg(feature = "proto-ipv6")]
pub(crate) enum MldReportState {
    Inactive,
    ToGeneralQuery {
        timeout: crate::time::Instant,
    },
    ToSpecificQuery {
        group: Ipv6Address,
        timeout: crate::time::Instant,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupState {
    /// Joining group, we have to send the join packet.
    Joining,
    /// We've already sent the join packet, we have nothing to do.
    Joined,
    /// We want to leave the group, we have to send a leave packet.
    Leaving,
}

pub(crate) struct State {
    groups: LinearMap<IpAddress, GroupState, IFACE_MAX_MULTICAST_GROUP_COUNT>,
    /// When to report for (all or) the next multicast group membership via IGMP
    #[cfg(feature = "proto-ipv4")]
    igmp_report_state: IgmpReportState,
    #[cfg(feature = "proto-ipv6")]
    mld_report_state: MldReportState,
}

impl State {
    pub(crate) fn new() -> Self {
        Self {
            groups: LinearMap::new(),
            #[cfg(feature = "proto-ipv4")]
            igmp_report_state: IgmpReportState::Inactive,
            #[cfg(feature = "proto-ipv6")]
            mld_report_state: MldReportState::Inactive,
        }
    }

    pub(crate) fn has_multicast_group<T: Into<IpAddress>>(&self, addr: T) -> bool {
        // Return false if we don't have the multicast group,
        // or we're leaving it.
        match self.groups.get(&addr.into()) {
            None => false,
            Some(GroupState::Joining) => true,
            Some(GroupState::Joined) => true,
            Some(GroupState::Leaving) => false,
        }
    }

    #[cfg(feature = "proto-ipv6")]
    fn schedule_mld_report(&mut self, group: Option<Ipv6Address>, timeout: crate::time::Instant) {
        let (group, timeout) = match self.mld_report_state {
            MldReportState::Inactive => (group, timeout),
            MldReportState::ToGeneralQuery {
                timeout: pending_timeout,
            } => (None, timeout.min(pending_timeout)),
            MldReportState::ToSpecificQuery {
                group: pending_group,
                timeout: pending_timeout,
            } => (
                (group == Some(pending_group)).then_some(pending_group),
                timeout.min(pending_timeout),
            ),
        };

        self.mld_report_state = match group {
            Some(group) => MldReportState::ToSpecificQuery { group, timeout },
            None => MldReportState::ToGeneralQuery { timeout },
        };
    }

    pub(super) fn poll_at(&self) -> Option<crate::time::Instant> {
        if self
            .groups
            .values()
            .any(|state| *state != GroupState::Joined)
        {
            return Some(crate::time::Instant::ZERO);
        }

        let mut result = None;
        #[cfg(feature = "proto-ipv4")]
        {
            let timeout = match self.igmp_report_state {
                IgmpReportState::Inactive => None,
                IgmpReportState::ToGeneralQuery { timeout, .. }
                | IgmpReportState::ToSpecificQuery { timeout, .. } => Some(timeout),
            };
            result = result.into_iter().chain(timeout).min();
        }
        #[cfg(feature = "proto-ipv6")]
        {
            let timeout = match self.mld_report_state {
                MldReportState::Inactive => None,
                MldReportState::ToGeneralQuery { timeout }
                | MldReportState::ToSpecificQuery { timeout, .. } => Some(timeout),
            };
            result = result.into_iter().chain(timeout).min();
        }
        result
    }
}

impl core::fmt::Display for MulticastError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            MulticastError::GroupTableFull => write!(f, "GroupTableFull"),
            MulticastError::Unaddressable => write!(f, "Unaddressable"),
        }
    }
}

impl core::error::Error for MulticastError {}

impl Interface {
    /// Add an address to a list of subscribed multicast IP addresses.
    pub fn join_multicast_group<T: Into<IpAddress>>(
        &mut self,
        addr: T,
    ) -> Result<(), MulticastError> {
        let addr = addr.into();
        if !addr.is_multicast() {
            return Err(MulticastError::Unaddressable);
        }

        if let Some(state) = self.inner.multicast.groups.get_mut(&addr) {
            *state = match state {
                GroupState::Joining => GroupState::Joining,
                GroupState::Joined => GroupState::Joined,
                GroupState::Leaving => GroupState::Joined,
            };
        } else {
            self.inner
                .multicast
                .groups
                .insert(addr, GroupState::Joining)
                .map_err(|_| MulticastError::GroupTableFull)?;
        }
        Ok(())
    }

    /// Remove an address from the subscribed multicast IP addresses.
    pub fn leave_multicast_group<T: Into<IpAddress>>(
        &mut self,
        addr: T,
    ) -> Result<(), MulticastError> {
        let addr = addr.into();
        if !addr.is_multicast() {
            return Err(MulticastError::Unaddressable);
        }

        if let Some(state) = self.inner.multicast.groups.get_mut(&addr) {
            let delete;
            (*state, delete) = match state {
                GroupState::Joining => (GroupState::Joined, true),
                GroupState::Joined => (GroupState::Leaving, false),
                GroupState::Leaving => (GroupState::Leaving, false),
            };
            if delete {
                self.inner.multicast.groups.remove(&addr);
            }
        }
        Ok(())
    }

    /// Check whether the interface listens to given destination multicast IP address.
    pub fn has_multicast_group<T: Into<IpAddress>>(&self, addr: T) -> bool {
        self.inner.has_multicast_group(addr)
    }

    #[cfg(feature = "proto-ipv6")]
    pub(super) fn update_solicited_node_groups(&mut self) {
        // Remove old solicited-node multicast addresses
        let removals: Vec<_, IFACE_MAX_MULTICAST_GROUP_COUNT> = self
            .inner
            .multicast
            .groups
            .keys()
            .cloned()
            .filter(|a| matches!(a, IpAddress::Ipv6(a) if a.is_solicited_node_multicast() && !self.inner.has_solicited_node(*a)))
            .collect();
        for removal in removals {
            let _ = self.leave_multicast_group(removal);
        }

        let cidrs: Vec<IpCidr, IFACE_MAX_ADDR_COUNT> = Vec::from_slice(self.ip_addrs()).unwrap();
        for cidr in cidrs {
            if let IpCidr::Ipv6(cidr) = cidr {
                let addr = cidr.address();
                if addr.x_is_unicast() && addr != Ipv6Address::LOCALHOST {
                    let _ = self.join_multicast_group(addr.solicited_node());
                }
            }
        }
    }

    /// Do multicast egress.
    ///
    /// - Send join/leave packets according to the multicast group state.
    /// - Send scheduled IGMP and MLD membership reports.
    pub(crate) fn multicast_egress(&mut self, device: &mut (impl Device + ?Sized)) {
        // Process multicast joins.
        while let Some((&addr, _)) = self
            .inner
            .multicast
            .groups
            .iter()
            .find(|&(_, &state)| state == GroupState::Joining)
        {
            match addr {
                #[cfg(feature = "proto-ipv4")]
                IpAddress::Ipv4(addr) => {
                    if let Some(pkt) = self.inner.igmp_report_packet(IgmpVersion::Version2, addr) {
                        let Some(tx_token) = device.transmit(self.inner.now) else {
                            break;
                        };

                        // NOTE(unwrap): packet destination is multicast, which is always routable and doesn't require neighbor discovery.
                        self.inner
                            .dispatch_ip(tx_token, PacketMeta::default(), pkt, &mut self.fragmenter)
                            .unwrap();
                    }
                }
                #[cfg(feature = "proto-ipv6")]
                IpAddress::Ipv6(addr) => {
                    let record = MldAddressRecordRepr::new(MldRecordType::ChangeToInclude, addr);
                    let pkt = self
                        .inner
                        .mldv2_report_packet(core::slice::from_ref(&record));
                    let Some(tx_token) = device.transmit(self.inner.now) else {
                        break;
                    };

                    // NOTE(unwrap): packet destination is multicast, which is always routable and doesn't require neighbor discovery.
                    self.inner
                        .dispatch_ip(tx_token, PacketMeta::default(), pkt, &mut self.fragmenter)
                        .unwrap();
                }
            }

            // NOTE(unwrap): this is always replacing an existing entry, so it can't fail due to the map being full.
            self.inner
                .multicast
                .groups
                .insert(addr, GroupState::Joined)
                .unwrap();
        }

        // Process multicast leaves.
        while let Some((&addr, _)) = self
            .inner
            .multicast
            .groups
            .iter()
            .find(|&(_, &state)| state == GroupState::Leaving)
        {
            match addr {
                #[cfg(feature = "proto-ipv4")]
                IpAddress::Ipv4(addr) => {
                    if let Some(pkt) = self.inner.igmp_leave_packet(addr) {
                        let Some(tx_token) = device.transmit(self.inner.now) else {
                            break;
                        };

                        // NOTE(unwrap): packet destination is multicast, which is always routable and doesn't require neighbor discovery.
                        self.inner
                            .dispatch_ip(tx_token, PacketMeta::default(), pkt, &mut self.fragmenter)
                            .unwrap();
                    }
                }
                #[cfg(feature = "proto-ipv6")]
                IpAddress::Ipv6(addr) => {
                    let record = MldAddressRecordRepr::new(MldRecordType::ChangeToExclude, addr);
                    let pkt = self
                        .inner
                        .mldv2_report_packet(core::slice::from_ref(&record));
                    let Some(tx_token) = device.transmit(self.inner.now) else {
                        break;
                    };

                    // NOTE(unwrap): packet destination is multicast, which is always routable and doesn't require neighbor discovery.
                    self.inner
                        .dispatch_ip(tx_token, PacketMeta::default(), pkt, &mut self.fragmenter)
                        .unwrap();
                }
            }

            self.inner.multicast.groups.remove(&addr);
        }

        #[cfg(feature = "proto-ipv4")]
        match self.inner.multicast.igmp_report_state {
            IgmpReportState::ToSpecificQuery {
                version,
                timeout,
                group,
            } if self.inner.now >= timeout => {
                if let Some(pkt) = self.inner.igmp_report_packet(version, group) {
                    // Send initial membership report
                    if let Some(tx_token) = device.transmit(self.inner.now) {
                        // NOTE(unwrap): packet destination is multicast, which is always routable and doesn't require neighbor discovery.
                        self.inner
                            .dispatch_ip(tx_token, PacketMeta::default(), pkt, &mut self.fragmenter)
                            .unwrap();
                        self.inner.multicast.igmp_report_state = IgmpReportState::Inactive;
                    }
                }
            }
            IgmpReportState::ToGeneralQuery {
                version,
                timeout,
                interval,
                next_index,
            } if self.inner.now >= timeout => {
                let addr = self
                    .inner
                    .multicast
                    .groups
                    .iter()
                    .filter_map(|(addr, _)| match addr {
                        IpAddress::Ipv4(addr) => Some(*addr),
                        #[allow(unreachable_patterns)]
                        _ => None,
                    })
                    .nth(next_index);

                match addr {
                    Some(addr) => {
                        if let Some(pkt) = self.inner.igmp_report_packet(version, addr) {
                            // Send initial membership report
                            if let Some(tx_token) = device.transmit(self.inner.now) {
                                // NOTE(unwrap): packet destination is multicast, which is always routable and doesn't require neighbor discovery.
                                self.inner
                                    .dispatch_ip(
                                        tx_token,
                                        PacketMeta::default(),
                                        pkt,
                                        &mut self.fragmenter,
                                    )
                                    .unwrap();

                                let next_timeout = (timeout + interval).max(self.inner.now);
                                self.inner.multicast.igmp_report_state =
                                    IgmpReportState::ToGeneralQuery {
                                        version,
                                        timeout: next_timeout,
                                        interval,
                                        next_index: next_index + 1,
                                    };
                            }
                        }
                    }
                    None => {
                        self.inner.multicast.igmp_report_state = IgmpReportState::Inactive;
                    }
                }
            }
            _ => {}
        }
        #[cfg(feature = "proto-ipv6")]
        match self.inner.multicast.mld_report_state {
            MldReportState::ToGeneralQuery { timeout } if self.inner.now >= timeout => {
                let records = self
                    .inner
                    .multicast
                    .groups
                    .iter()
                    .filter_map(|(addr, state)| match (addr, *state) {
                        (IpAddress::Ipv6(addr), GroupState::Joining | GroupState::Joined) => Some(
                            MldAddressRecordRepr::new(MldRecordType::ModeIsExclude, *addr),
                        ),
                        #[allow(unreachable_patterns)]
                        _ => None,
                    })
                    .collect::<heapless::Vec<_, IFACE_MAX_MULTICAST_GROUP_COUNT>>();
                if records.is_empty() {
                    self.inner.multicast.mld_report_state = MldReportState::Inactive;
                } else if let Some(tx_token) = device.transmit(self.inner.now) {
                    let pkt = self.inner.mldv2_report_packet(&records);
                    self.inner
                        .dispatch_ip(tx_token, PacketMeta::default(), pkt, &mut self.fragmenter)
                        .unwrap();
                    self.inner.multicast.mld_report_state = MldReportState::Inactive;
                }
            }
            MldReportState::ToSpecificQuery { group, timeout } if self.inner.now >= timeout => {
                if !self.inner.multicast.has_multicast_group(group) {
                    self.inner.multicast.mld_report_state = MldReportState::Inactive;
                } else if let Some(tx_token) = device.transmit(self.inner.now) {
                    let record = MldAddressRecordRepr::new(MldRecordType::ModeIsExclude, group);
                    let pkt = self
                        .inner
                        .mldv2_report_packet(core::slice::from_ref(&record));
                    // NOTE(unwrap): packet destination is multicast, which is always routable and doesn't require neighbor discovery.
                    self.inner
                        .dispatch_ip(tx_token, PacketMeta::default(), pkt, &mut self.fragmenter)
                        .unwrap();
                    self.inner.multicast.mld_report_state = MldReportState::Inactive;
                }
            }
            _ => {}
        }
    }
}

impl InterfaceInner {
    /// Host duties of the **IGMPv2** protocol.
    ///
    /// Schedules delayed reports for general and group-specific membership queries.
    #[cfg(feature = "proto-ipv4")]
    pub(super) fn process_igmp<'frame>(
        &mut self,
        ipv4_repr: Ipv4Repr,
        ip_payload: &'frame [u8],
    ) -> Option<Packet<'frame>> {
        use crate::time::Duration;

        let igmp_packet = check!(IgmpPacket::new_checked(ip_payload));
        let igmp_repr = check!(IgmpRepr::parse(&igmp_packet));

        match igmp_repr {
            IgmpRepr::MembershipQuery {
                group_addr,
                version,
                max_resp_time,
            } => {
                // General query
                if group_addr.is_unspecified() && ipv4_repr.dst_addr == IPV4_MULTICAST_ALL_SYSTEMS {
                    let ipv4_multicast_group_count = self
                        .multicast
                        .groups
                        .keys()
                        .filter(|a| matches!(a, IpAddress::Ipv4(_)))
                        .count();

                    // Are we member in any groups?
                    if ipv4_multicast_group_count != 0 {
                        let interval = match version {
                            IgmpVersion::Version1 => Duration::from_millis(100),
                            IgmpVersion::Version2 => {
                                // No dependence on a random generator
                                // (see [#24](https://github.com/m-labs/smoltcp/issues/24))
                                // but at least spread reports evenly across max_resp_time.
                                let intervals = ipv4_multicast_group_count as u32 + 1;
                                max_resp_time / intervals
                            }
                        };
                        self.multicast.igmp_report_state = IgmpReportState::ToGeneralQuery {
                            version,
                            timeout: self.now + interval,
                            interval,
                            next_index: 0,
                        };
                    }
                } else {
                    // Group-specific query
                    if self.has_multicast_group(group_addr) && ipv4_repr.dst_addr == group_addr {
                        // Don't respond immediately
                        let timeout = max_resp_time / 4;
                        self.multicast.igmp_report_state = IgmpReportState::ToSpecificQuery {
                            version,
                            timeout: self.now + timeout,
                            group: group_addr,
                        };
                    }
                }
            }
            // Ignore membership reports
            IgmpRepr::MembershipReport { .. } => (),
            // Ignore hosts leaving groups
            IgmpRepr::LeaveGroup { .. } => (),
        }

        None
    }

    #[cfg(feature = "proto-ipv4")]
    fn igmp_report_packet<'any>(
        &self,
        version: IgmpVersion,
        group_addr: Ipv4Address,
    ) -> Option<Packet<'any>> {
        let iface_addr = self.ipv4_addr()?;
        let igmp_repr = IgmpRepr::MembershipReport {
            group_addr,
            version,
        };
        let pkt = Packet::new_ipv4(
            Ipv4Repr {
                src_addr: iface_addr,
                // Send to the group being reported
                dst_addr: group_addr,
                next_header: IpProtocol::Igmp,
                payload_len: igmp_repr.buffer_len(),
                hop_limit: 1,
                // [#183](https://github.com/m-labs/smoltcp/issues/183).
            },
            IpPayload::Igmp(igmp_repr),
        );
        Some(pkt)
    }

    #[cfg(feature = "proto-ipv4")]
    fn igmp_leave_packet<'any>(&self, group_addr: Ipv4Address) -> Option<Packet<'any>> {
        self.ipv4_addr().map(|iface_addr| {
            let igmp_repr = IgmpRepr::LeaveGroup { group_addr };
            Packet::new_ipv4(
                Ipv4Repr {
                    src_addr: iface_addr,
                    dst_addr: IPV4_MULTICAST_ALL_ROUTERS,
                    next_header: IpProtocol::Igmp,
                    payload_len: igmp_repr.buffer_len(),
                    hop_limit: 1,
                },
                IpPayload::Igmp(igmp_repr),
            )
        })
    }

    /// Host duties of the **MLDv2** protocol.
    ///
    /// Sets up `mld_report_state` for responding to MLD general/specific membership queries.
    /// Reports are delayed to avoid flooding the network after a router broadcasts a query.
    #[cfg(feature = "proto-ipv6")]
    pub(super) fn process_mldv2<'frame>(
        &mut self,
        ip_repr: Ipv6Repr,
        repr: MldRepr<'frame>,
    ) -> Option<Packet<'frame>> {
        match repr {
            MldRepr::Query {
                mcast_addr,
                max_resp_code,
                ..
            } => {
                let max_delay = mldv2_max_resp_delay(max_resp_code).total_millis();
                let delay = if max_delay == 0 {
                    crate::time::Duration::ZERO
                } else {
                    crate::time::Duration::from_millis(
                        1 + u64::from(self.rand.rand_u32()) % max_delay,
                    )
                };
                // General query
                if mcast_addr.is_unspecified()
                    && (ip_repr.dst_addr == IPV6_LINK_LOCAL_ALL_NODES
                        || self.has_ip_addr(ip_repr.dst_addr))
                {
                    let ipv6_multicast_group_count = self
                        .multicast
                        .groups
                        .keys()
                        .filter(|a| matches!(a, IpAddress::Ipv6(_)))
                        .count();
                    if ipv6_multicast_group_count != 0 {
                        self.multicast.schedule_mld_report(None, self.now + delay);
                    }
                }
                if self.has_multicast_group(mcast_addr) && ip_repr.dst_addr == mcast_addr {
                    self.multicast
                        .schedule_mld_report(Some(mcast_addr), self.now + delay);
                }
                None
            }
            MldRepr::Report { .. } => None,
            MldRepr::ReportRecordReprs { .. } => None,
        }
    }
}

#[cfg(all(test, feature = "proto-ipv6", feature = "medium-ethernet"))]
mod tests {
    use super::*;
    use crate::phy::{DeviceCapabilities, Medium};
    use crate::rand::Rand;
    use crate::tests::setup;

    const GROUP_A: Ipv6Address = Ipv6Address::new(0xff02, 0, 0, 0, 0, 0, 0, 0x1234);
    const GROUP_B: Ipv6Address = Ipv6Address::new(0xff02, 0, 0, 0, 0, 0, 0, 0x5678);

    struct TxUnavailable {
        caps: DeviceCapabilities,
        transmit_calls: usize,
    }

    impl Device for TxUnavailable {
        type RxToken<'a> = crate::tests::RxToken;
        type TxToken<'a> = crate::tests::TxToken<'a>;

        fn capabilities(&self) -> DeviceCapabilities {
            self.caps.clone()
        }

        fn receive(
            &mut self,
            _timestamp: crate::time::Instant,
        ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
            None
        }

        fn transmit(&mut self, _timestamp: crate::time::Instant) -> Option<Self::TxToken<'_>> {
            self.transmit_calls += 1;
            None
        }
    }

    fn assert_general(state: &State, timeout: crate::time::Instant) {
        assert!(matches!(
            state.mld_report_state,
            MldReportState::ToGeneralQuery { timeout: actual } if actual == timeout
        ));
    }

    fn assert_specific(state: &State, group: Ipv6Address, timeout: crate::time::Instant) {
        assert!(matches!(
            state.mld_report_state,
            MldReportState::ToSpecificQuery {
                group: actual_group,
                timeout: actual_timeout,
            } if actual_group == group && actual_timeout == timeout
        ));
    }

    fn scheduled_delay(max_resp_code: u16) -> u64 {
        let (mut iface, _, _) = setup(Medium::Ethernet);
        let now = crate::time::Instant::from_millis(10);
        iface.inner.now = now;

        let repr = MldRepr::Query {
            max_resp_code,
            mcast_addr: Ipv6Address::UNSPECIFIED,
            s_flag: false,
            qrv: 0,
            qqic: 0,
            num_srcs: 0,
            data: &[],
        };
        iface.inner.rand = Rand::new(1);
        iface.inner.process_mldv2(
            Ipv6Repr {
                src_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2),
                dst_addr: IPV6_LINK_LOCAL_ALL_NODES,
                next_header: IpProtocol::Icmpv6,
                payload_len: repr.buffer_len(),
                hop_limit: 1,
            },
            repr,
        );

        match iface.inner.multicast.mld_report_state {
            MldReportState::ToGeneralQuery { timeout } => (timeout - now).total_millis(),
            _ => panic!("MLD report was not scheduled"),
        }
    }

    #[test]
    fn mld_query_delay_uses_decoded_range() {
        assert_eq!(scheduled_delay(0xffff), 8_282_290);
        assert_eq!(scheduled_delay(1), 1);
        assert_eq!(scheduled_delay(0), 0);
    }

    #[test]
    fn mld_report_schedule_merges_pending_queries() {
        let earlier = crate::time::Instant::from_millis(10);
        let later = crate::time::Instant::from_millis(20);

        let mut state = State::new();
        state.schedule_mld_report(Some(GROUP_A), later);
        state.schedule_mld_report(Some(GROUP_A), earlier);
        assert_specific(&state, GROUP_A, earlier);

        let mut state = State::new();
        state.schedule_mld_report(Some(GROUP_A), later);
        state.schedule_mld_report(None, earlier);
        assert_general(&state, earlier);
        state.schedule_mld_report(Some(GROUP_A), later);
        assert_general(&state, earlier);

        let mut state = State::new();
        state.schedule_mld_report(Some(GROUP_A), later);
        state.schedule_mld_report(Some(GROUP_B), earlier);
        assert_general(&state, earlier);
    }

    #[test]
    fn multicast_poll_at_includes_state_work() {
        let mld_timeout = crate::time::Instant::from_millis(20);
        let mut state = State::new();
        state
            .groups
            .insert(GROUP_A.into(), GroupState::Joined)
            .unwrap();
        assert_eq!(state.poll_at(), None);

        state.mld_report_state = MldReportState::ToGeneralQuery {
            timeout: mld_timeout,
        };
        assert_eq!(state.poll_at(), Some(mld_timeout));

        #[cfg(feature = "proto-ipv4")]
        {
            let igmp_timeout = crate::time::Instant::from_millis(10);
            state.igmp_report_state = IgmpReportState::ToSpecificQuery {
                version: IgmpVersion::Version2,
                timeout: igmp_timeout,
                group: Ipv4Address::new(224, 0, 0, 1),
            };
            assert_eq!(state.poll_at(), Some(igmp_timeout));
        }

        state
            .groups
            .insert(GROUP_A.into(), GroupState::Joining)
            .unwrap();
        assert_eq!(state.poll_at(), Some(crate::time::Instant::ZERO));
        state
            .groups
            .insert(GROUP_A.into(), GroupState::Leaving)
            .unwrap();
        assert_eq!(state.poll_at(), Some(crate::time::Instant::ZERO));
    }

    #[test]
    fn mld_due_report_survives_tx_unavailable() {
        let (mut iface, mut sockets, mut device) = setup(Medium::Ethernet);
        let now = crate::time::Instant::from_millis(10);
        iface.inner.multicast.groups.clear();
        iface
            .inner
            .multicast
            .groups
            .insert(GROUP_A.into(), GroupState::Joined)
            .unwrap();
        iface.inner.multicast.mld_report_state = MldReportState::ToSpecificQuery {
            group: GROUP_A,
            timeout: now,
        };

        let mut unavailable = TxUnavailable {
            caps: device.capabilities(),
            transmit_calls: 0,
        };
        iface.poll_egress(now, &mut unavailable, &mut sockets);
        assert_eq!(unavailable.transmit_calls, 1);
        assert_specific(&iface.inner.multicast, GROUP_A, now);
        assert_eq!(iface.poll_at(now, &sockets), Some(now));

        iface.poll_egress(now, &mut device, &mut sockets);
        assert_eq!(device.tx_queue.len(), 1);
        assert!(matches!(
            iface.inner.multicast.mld_report_state,
            MldReportState::Inactive
        ));
        iface.poll_egress(now, &mut device, &mut sockets);
        assert_eq!(device.tx_queue.len(), 1);
    }

    #[test]
    fn mld_stale_reports_clear_without_transmit() {
        let (mut iface, mut sockets, device) = setup(Medium::Ethernet);
        let now = crate::time::Instant::from_millis(10);
        iface.inner.multicast.groups.clear();

        iface.inner.multicast.mld_report_state = MldReportState::ToSpecificQuery {
            group: GROUP_A,
            timeout: now,
        };
        let mut unavailable = TxUnavailable {
            caps: device.capabilities(),
            transmit_calls: 0,
        };
        iface.poll_egress(now, &mut unavailable, &mut sockets);
        assert_eq!(unavailable.transmit_calls, 0);
        assert!(matches!(
            iface.inner.multicast.mld_report_state,
            MldReportState::Inactive
        ));

        iface.inner.multicast.mld_report_state = MldReportState::ToGeneralQuery { timeout: now };
        iface.poll_egress(now, &mut unavailable, &mut sockets);
        assert_eq!(unavailable.transmit_calls, 0);
        assert!(matches!(
            iface.inner.multicast.mld_report_state,
            MldReportState::Inactive
        ));
    }
}
