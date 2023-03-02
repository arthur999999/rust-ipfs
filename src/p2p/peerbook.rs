/// PeerBook with connection limits based on https://github.com/libp2p/rust-libp2p/pull/3386
use core::task::{Context, Poll};
use futures::StreamExt;
use libp2p::core::{Endpoint, Multiaddr};
use libp2p::swarm::{
    self, dummy::ConnectionHandler as DummyConnectionHandler, NetworkBehaviour, PollParameters,
};
use libp2p::swarm::{
    ConnectionClosed, ConnectionDenied, ConnectionId, ConnectionLimit, FromSwarm, THandler,
    THandlerInEvent,
};
use libp2p::PeerId;
use std::collections::hash_map::Entry;
use std::time::Duration;
use wasm_timer::Interval;

use std::collections::{HashMap, HashSet, VecDeque};

use super::PeerInfo;

#[derive(Debug, Clone, Copy, Default)]
pub struct ConnectionLimits {
    max_pending_incoming: Option<u32>,
    max_pending_outgoing: Option<u32>,
    max_established_incoming: Option<u32>,
    max_established_outgoing: Option<u32>,
    max_established_per_peer: Option<u32>,
    max_established_total: Option<u32>,
}

impl ConnectionLimits {
    pub fn max_pending_incoming(&self) -> Option<u32> {
        self.max_pending_incoming
    }

    pub fn max_pending_outgoing(&self) -> Option<u32> {
        self.max_pending_outgoing
    }

    pub fn max_established_incoming(&self) -> Option<u32> {
        self.max_established_incoming
    }

    pub fn max_established_outgoing(&self) -> Option<u32> {
        self.max_established_outgoing
    }

    pub fn max_established(&self) -> Option<u32> {
        self.max_established_total
    }

    pub fn max_established_per_peer(&self) -> Option<u32> {
        self.max_established_per_peer
    }
}

impl ConnectionLimits {
    pub fn set_max_pending_incoming(&mut self, limit: Option<u32>) {
        self.max_pending_incoming = limit;
    }

    pub fn set_max_pending_outgoing(&mut self, limit: Option<u32>) {
        self.max_pending_outgoing = limit;
    }

    pub fn set_max_established_incoming(&mut self, limit: Option<u32>) {
        self.max_established_incoming = limit;
    }

    pub fn set_max_established_outgoing(&mut self, limit: Option<u32>) {
        self.max_established_outgoing = limit;
    }

    pub fn set_max_established(&mut self, limit: Option<u32>) {
        self.max_established_total = limit;
    }

    pub fn set_max_established_per_peer(&mut self, limit: Option<u32>) {
        self.max_established_per_peer = limit;
    }
}

impl ConnectionLimits {
    pub fn with_max_pending_incoming(mut self, limit: Option<u32>) -> Self {
        self.max_pending_incoming = limit;
        self
    }

    pub fn with_max_pending_outgoing(mut self, limit: Option<u32>) -> Self {
        self.max_pending_outgoing = limit;
        self
    }

    pub fn with_max_established_incoming(mut self, limit: Option<u32>) -> Self {
        self.max_established_incoming = limit;
        self
    }

    pub fn with_max_established_outgoing(mut self, limit: Option<u32>) -> Self {
        self.max_established_outgoing = limit;
        self
    }

    pub fn with_max_established(mut self, limit: Option<u32>) -> Self {
        self.max_established_total = limit;
        self
    }

    pub fn with_max_established_per_peer(mut self, limit: Option<u32>) -> Self {
        self.max_established_per_peer = limit;
        self
    }
}

#[derive(Debug)]
#[allow(clippy::type_complexity)]
pub struct Behaviour {
    limits: ConnectionLimits,

    events: VecDeque<
        swarm::NetworkBehaviourAction<<Self as NetworkBehaviour>::OutEvent, THandlerInEvent<Self>>,
    >,
    cleanup_interval: Interval,

    peer_info: HashMap<PeerId, PeerInfo>,
    peer_rtt: HashMap<PeerId, [Duration; 3]>,

    whitelist: HashSet<PeerId>,

    // For connection limits (took from )
    pending_inbound_connections: HashSet<ConnectionId>,
    pending_outbound_connections: HashSet<ConnectionId>,
    established_inbound_connections: HashSet<ConnectionId>,
    established_outbound_connections: HashSet<ConnectionId>,
    established_per_peer: HashMap<PeerId, HashSet<ConnectionId>>,
}

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            limits: Default::default(),
            events: Default::default(),
            cleanup_interval: Interval::new_at(
                std::time::Instant::now() + Duration::from_secs(60),
                Duration::from_secs(60),
            ),
            peer_info: Default::default(),
            peer_rtt: Default::default(),
            whitelist: Default::default(),
            pending_inbound_connections: Default::default(),
            pending_outbound_connections: Default::default(),
            established_inbound_connections: Default::default(),
            established_outbound_connections: Default::default(),
            established_per_peer: Default::default(),
        }
    }
}

impl Behaviour {
    pub fn set_connection_limit(&mut self, limit: ConnectionLimits) {
        self.limits = limit;
    }

    pub fn add(&mut self, peer_id: PeerId) {
        self.whitelist.insert(peer_id);
    }

    pub fn remove(&mut self, peer_id: PeerId) {
        self.whitelist.remove(&peer_id);
    }

    pub fn inject_peer_info<I: Into<PeerInfo>>(&mut self, info: I) {
        let info = info.into();
        self.peer_info.insert(info.peer_id, info);
    }

    pub fn peers(&self) -> impl Iterator<Item = &PeerId> {
        self.peer_info.keys()
    }

    pub fn set_peer_rtt(&mut self, peer_id: PeerId, rtt: Duration) {
        if self.peer_info.contains_key(&peer_id) {
            self.peer_rtt
                .entry(peer_id)
                .and_modify(|r| {
                    r.rotate_left(1);
                    r[2] = rtt;
                })
                .or_insert([Duration::from_millis(0), Duration::from_millis(0), rtt]);
        }
    }

    pub fn get_peer_info(&self, peer_id: PeerId) -> Option<&PeerInfo> {
        self.peer_info.get(&peer_id)
    }

    pub fn remove_peer_info(&mut self, peer_id: PeerId) {
        self.peer_info.remove(&peer_id);
    }

    fn check_limit(&mut self, limit: Option<u32>, current: usize) -> Result<(), ConnectionDenied> {
        let limit = limit.unwrap_or(u32::MAX);
        let current = current as u32;

        if current >= limit {
            return Err(ConnectionDenied::new(ConnectionLimit { limit, current }));
        }

        Ok(())
    }
}

impl NetworkBehaviour for Behaviour {
    type ConnectionHandler = DummyConnectionHandler;
    type OutEvent = void::Void;

    fn handle_pending_inbound_connection(
        &mut self,
        connection_id: ConnectionId,
        _: &Multiaddr,
        _: &Multiaddr,
    ) -> Result<(), ConnectionDenied> {
        self.check_limit(
            self.limits.max_pending_incoming,
            self.pending_inbound_connections.len(),
        )?;

        self.pending_inbound_connections.insert(connection_id);

        Ok(())
    }

    fn handle_pending_outbound_connection(
        &mut self,
        connection_id: ConnectionId,
        peer_id: Option<PeerId>,
        _: &[Multiaddr],
        _: Endpoint,
    ) -> Result<Vec<Multiaddr>, ConnectionDenied> {
        let mut is_whitelisted = false;

        if let Some(peer_id) = peer_id {
            is_whitelisted = self.whitelist.contains(&peer_id);
        }

        if !is_whitelisted {
            self.check_limit(
                self.limits.max_pending_outgoing,
                self.pending_outbound_connections.len(),
            )?;
        }

        self.pending_outbound_connections.insert(connection_id);

        Ok(vec![])
    }

    fn handle_established_inbound_connection(
        &mut self,
        connection_id: ConnectionId,
        peer_id: PeerId,
        _: &Multiaddr,
        _: &Multiaddr,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        self.pending_inbound_connections.remove(&connection_id);

        if !self.whitelist.contains(&peer_id) {
            self.check_limit(
                self.limits.max_established_incoming,
                self.established_inbound_connections.len(),
            )?;
            self.check_limit(
                self.limits.max_established_per_peer,
                self.established_per_peer
                    .get(&peer_id)
                    .map(|connections| connections.len())
                    .unwrap_or(0),
            )?;
            self.check_limit(
                self.limits.max_established_total,
                self.established_inbound_connections.len()
                    + self.established_outbound_connections.len(),
            )?;
        }

        self.established_inbound_connections.insert(connection_id);
        self.established_per_peer
            .entry(peer_id)
            .or_default()
            .insert(connection_id);

        Ok(DummyConnectionHandler)
    }

    fn handle_established_outbound_connection(
        &mut self,
        connection_id: ConnectionId,
        peer_id: PeerId,
        _: &Multiaddr,
        _: Endpoint,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        self.pending_outbound_connections.remove(&connection_id);

        if !self.whitelist.contains(&peer_id) {
            self.check_limit(
                self.limits.max_established_outgoing,
                self.established_outbound_connections.len(),
            )?;
            self.check_limit(
                self.limits.max_established_per_peer,
                self.established_per_peer
                    .get(&peer_id)
                    .map(|connections| connections.len())
                    .unwrap_or(0),
            )?;
            self.check_limit(
                self.limits.max_established_total,
                self.established_inbound_connections.len()
                    + self.established_outbound_connections.len(),
            )?;
        }

        self.established_outbound_connections.insert(connection_id);
        self.established_per_peer
            .entry(peer_id)
            .or_default()
            .insert(connection_id);

        Ok(DummyConnectionHandler)
    }

    fn on_connection_handler_event(
        &mut self,
        _: libp2p::PeerId,
        _: swarm::ConnectionId,
        _: swarm::THandlerOutEvent<Self>,
    ) {
    }

    #[allow(clippy::single_match)]
    fn on_swarm_event(&mut self, event: swarm::FromSwarm<Self::ConnectionHandler>) {
        match event {
            FromSwarm::ConnectionClosed(ConnectionClosed {
                peer_id,
                connection_id,
                ..
            }) => {
                self.established_inbound_connections.remove(&connection_id);
                self.established_outbound_connections.remove(&connection_id);
                if let Entry::Occupied(mut entry) = self.established_per_peer.entry(peer_id) {
                    entry.get_mut().remove(&connection_id);
                    if entry.get().is_empty() {
                        entry.remove();
                    }
                }
            }
            _ => {}
        }
    }

    fn poll(
        &mut self,
        cx: &mut Context,
        _: &mut impl PollParameters,
    ) -> Poll<swarm::NetworkBehaviourAction<Self::OutEvent, THandlerInEvent<Self>>> {
        if let Some(event) = self.events.pop_front() {
            return Poll::Ready(event);
        }

        // Used to cleanup any info that may be left behind after a peer is no longer connected while giving time to all
        // Note: If a peer is whitelisted, this will retain the info as a cache, although this may change in the future
        //
        while let Poll::Ready(Some(_)) = self.cleanup_interval.poll_next_unpin(cx) {
            let list = self.peer_info.keys().copied().collect::<Vec<_>>();
            for peer_id in list {
                if !self.established_per_peer.contains_key(&peer_id)
                    && !self.whitelist.contains(&peer_id)
                {
                    self.peer_info.remove(&peer_id);
                    self.peer_rtt.remove(&peer_id);
                }
            }
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use super::Behaviour as PeerBook;
    use crate::p2p::{peerbook::ConnectionLimits, transport::build_transport};
    use futures::StreamExt;
    use libp2p::{
        identity::Keypair,
        swarm::{keep_alive, NetworkBehaviour, SwarmBuilder, SwarmEvent},
        Multiaddr, PeerId, Swarm,
    };

    #[derive(NetworkBehaviour)]
    struct Behaviour {
        peerbook: PeerBook,
        keep_alive: keep_alive::Behaviour,
    }

    //TODO: Expand test out
    #[tokio::test]
    async fn connection_limits() {
        let (_, addr1, mut swarm1) = build_swarm().await;
        let (peer2, _, mut swarm2) = build_swarm().await;
        let (peer3, _, mut swarm3) = build_swarm().await;
        let (peer4, _, mut swarm4) = build_swarm().await;

        swarm1
            .behaviour_mut()
            .peerbook
            .set_connection_limit(ConnectionLimits {
                max_established_incoming: Some(1),
                ..Default::default()
            });

        swarm2.dial(addr1.clone()).unwrap();

        loop {
            if let Some(SwarmEvent::ConnectionEstablished { .. }) = swarm1.next().await {
                break;
            }
        }
        swarm1.behaviour_mut().peerbook.add(peer3);
        swarm3.dial(addr1.clone()).unwrap();

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(SwarmEvent::ConnectionEstablished { .. }) = swarm1.next().await {
                    break;
                }
            }
        })
        .await
        .unwrap();

        swarm4.dial(addr1.clone()).unwrap();

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(SwarmEvent::IncomingConnectionError { .. }) = swarm1.next().await {
                    break;
                }
            }
        })
        .await
        .unwrap();

        let list = swarm1.connected_peers().copied().collect::<Vec<_>>();

        assert!(list.contains(&peer2));
        assert!(list.contains(&peer3));
        assert!(!list.contains(&peer4));
    }

    async fn build_swarm() -> (PeerId, Multiaddr, libp2p::swarm::Swarm<Behaviour>) {
        let key = Keypair::generate_ed25519();
        let peer_id = key.public().to_peer_id();
        let transport = build_transport(key, None, Default::default()).unwrap();

        let behaviour = Behaviour {
            peerbook: PeerBook::default(),
            keep_alive: keep_alive::Behaviour,
        };

        let mut swarm = SwarmBuilder::with_tokio_executor(transport, behaviour, peer_id).build();

        Swarm::listen_on(&mut swarm, "/ip4/127.0.0.1/tcp/0".parse().unwrap()).unwrap();

        if let Some(SwarmEvent::NewListenAddr { address, .. }) = swarm.next().await {
            return (peer_id, address, swarm);
        }

        panic!("no new addrs")
    }
}