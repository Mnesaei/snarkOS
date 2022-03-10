// Copyright (C) 2019-2022 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

use parking_lot::RwLock;
use snarkos_environment::helpers::{NodeType, State};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    net::SocketAddr,
};
use time::{Duration, OffsetDateTime};

use crate::{
    connection::{nodes_from_connections, Connection},
    constants::*,
};

/// The current state of a crawled node.
#[derive(Debug, Clone)]
pub struct NodeState {
    node_type: NodeType,
    version: u32,
    height: u32,
    state: State,
}

/// A summary of the state of the known nodes.
#[derive(Clone)]
#[allow(dead_code)]
pub struct NetworkSummary {
    // The number of all known nodes.
    num_known_nodes: usize,
    // The number of all known connections.
    num_known_connections: usize,
    // The number of nodes that haven't provided their state yet.
    nodes_pending_state: usize,
    // The types of nodes and their respective counts.
    types: HashMap<NodeType, usize>,
    // The versions of nodes and their respective counts.
    versions: HashMap<u32, usize>,
    // The node states of nodes and their respective counts.
    states: HashMap<State, usize>,
    // The heights of nodes and their respective counts.
    heights: HashMap<u32, usize>,
    // The average handshake time in the network.
    avg_handshake_time_ms: Option<i64>,
}

impl fmt::Debug for NetworkSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Network summary")
            .field("number of known nodes", &self.num_known_nodes)
            .field("number of known connections", &self.num_known_connections)
            .field("nodes pending state", &self.nodes_pending_state)
            .field("types", &self.types)
            .field("versions", &self.versions)
            .field("states", &self.states)
            .field("average handshake time (in ms)", &self.avg_handshake_time_ms)
            .finish()
    }
}

/// Node information collected while crawling.
#[derive(Debug, Clone)]
pub struct NodeMeta {
    #[allow(dead_code)]
    listening_addr: SocketAddr,
    // The details of the node's state.
    pub state: Option<NodeState>,
    // The last interaction timestamp.
    timestamp: Option<OffsetDateTime>,
    // The number of lists of peers received from the node.
    received_peer_sets: u8,
    // The number of subsequent connection failures.
    connection_failures: u8,
    // The time it took to connect to the node.
    handshake_time: Option<Duration>,
}

impl NodeMeta {
    // Creates a new `NodeMeta` object.
    fn new(listening_addr: SocketAddr) -> Self {
        Self {
            listening_addr,
            state: None,
            timestamp: None,
            received_peer_sets: 0,
            connection_failures: 0,
            handshake_time: None,
        }
    }

    // Resets the node's values which determine whether the crawler should stay connected to it.
    // note: it should be called when a node is disconnected from after it's been crawled successfully
    fn reset_crawl_state(&mut self) {
        self.received_peer_sets = 0;
        self.connection_failures = 0;
        self.timestamp = Some(OffsetDateTime::now_utc());
    }

    // Returns `true` if the node should be connected to again.
    fn needs_refreshing(&self) -> bool {
        if let Some(timestamp) = self.timestamp {
            let crawl_interval = if self.state.is_some() {
                CRAWL_INTERVAL_MINS
            } else {
                // Delay further connection attempts to nodes that are hard to connect to.
                self.connection_failures as i64
            };

            (OffsetDateTime::now_utc() - timestamp).whole_minutes() > crawl_interval
        } else {
            // If there is no timestamp yet, this is the very first connection attempt.
            true
        }
    }
}

/// Keeps track of crawled peers and their connections.
// note: all the associated addresses are listening addresses.
#[derive(Debug, Default)]
pub struct KnownNetwork {
    // The information on known nodes; the keys of the map are their related listening addresses.
    nodes: RwLock<HashMap<SocketAddr, NodeMeta>>,
    // The map of known connections between nodes.
    connections: RwLock<HashSet<Connection>>,
}

impl KnownNetwork {
    /// Adds a node with the given address.
    pub fn add_node(&self, listening_addr: SocketAddr) {
        self.nodes.write().insert(listening_addr, NodeMeta::new(listening_addr));
    }

    // Updates the list of connections and registers new nodes based on them.
    fn update_connections(&self, source: SocketAddr, peers: Vec<SocketAddr>) {
        // Rules:
        //  - if a connecton exists already, do nothing.
        //  - if a connection is new, add it.
        //  - if an exisitng connection involving the source isn't in the peerlist, remove it if
        //  it's stale.

        let new_connections: HashSet<Connection> = peers.into_iter().map(|peer| Connection::new(source, peer)).collect();

        // Find which connections need to be removed.
        //
        // With sets: a - b = removed connections (if and only if one of the two addrs is the
        // source), otherwise it's a connection which doesn't include the source and shouldn't be
        // removed. We also keep connections seen within the last few hours as peerlists are capped
        // in size and omitted connections don't necessarily mean they don't exist anymore.
        let connections_to_remove: HashSet<Connection> = self
            .connections
            .read()
            .difference(&new_connections)
            .filter(|conn| {
                (conn.source == source || conn.target == source)
                    && (OffsetDateTime::now_utc() - conn.last_seen).whole_hours() > STALE_CONNECTION_CUTOFF_TIME_HRS
            })
            .copied()
            .collect();

        // Scope the write lock.
        {
            let mut connections_g = self.connections.write();

            // Remove stale connections, if there are any.
            if !connections_to_remove.is_empty() {
                connections_g.retain(|connection| !connections_to_remove.contains(connection));
            }

            // Insert new connections, we use replace so the last seen timestamp is overwritten.
            for new_connection in new_connections.into_iter() {
                connections_g.replace(new_connection);
            }
        }

        // Scope the write lock.
        {
            let mut nodes_g = self.nodes.write();

            // Remove the nodes that no longer correspond to connections.
            let nodes_from_connections = nodes_from_connections(&self.connections());
            for addr in nodes_from_connections {
                if !nodes_g.contains_key(&addr) {
                    nodes_g.insert(addr, NodeMeta::new(addr));
                }
            }
        }
    }

    /// Updates the details of a node based on a Ping message received from it.
    pub fn received_ping(&self, source: SocketAddr, node_type: NodeType, version: u32, state: State, height: u32) {
        let timestamp = OffsetDateTime::now_utc();

        let mut nodes = self.nodes.write();
        let mut meta = nodes.entry(source).or_insert_with(|| NodeMeta::new(source));

        meta.state = Some(NodeState {
            node_type,
            version,
            height,
            state,
        });
        meta.timestamp = Some(timestamp);
    }

    /// Updates the known connections based on a received list of a node's peers.
    pub fn received_peers(&self, source: SocketAddr, addrs: Vec<SocketAddr>) {
        let timestamp = OffsetDateTime::now_utc();

        self.update_connections(source, addrs);

        let mut nodes = self.nodes.write();
        let mut meta = nodes.entry(source).or_insert_with(|| NodeMeta::new(source));

        meta.received_peer_sets += 1;
        meta.timestamp = Some(timestamp);
    }

    /// Updates a node's details applicable as soon as a connection succeeds or fails.
    pub fn connected_to_node(&self, source: SocketAddr, connection_init_timestamp: OffsetDateTime, connection_succeeded: bool) {
        let mut nodes = self.nodes.write();
        let mut meta = nodes.entry(source).or_insert_with(|| NodeMeta::new(source));

        // Update the node interaction timestamp.
        meta.timestamp = Some(connection_init_timestamp);

        if connection_succeeded {
            // Reset the conn failure count when the connection succeeds.
            meta.connection_failures = 0;
            // Register the time it took to perform the handshake.
            meta.handshake_time = Some(OffsetDateTime::now_utc() - connection_init_timestamp);
        } else {
            meta.connection_failures += 1;
        }
    }

    /// Checks if the given address should be (re)connected to.
    pub fn should_be_connected_to(&self, addr: SocketAddr) -> bool {
        if let Some(meta) = self.nodes.read().get(&addr) {
            meta.needs_refreshing()
        } else {
            true
        }
    }

    /// Returns a list of addresses the crawler should connect to.
    pub fn addrs_to_connect(&self) -> HashSet<SocketAddr> {
        // Snapshot is safe to use as disconnected peers won't have their state updated at the
        // moment.
        self.nodes()
            .iter()
            .filter(|(_, meta)| meta.needs_refreshing())
            .map(|(&addr, _)| addr)
            .collect()
    }

    /// Returns a list of addresses the crawler should disconnect from.
    pub fn addrs_to_disconnect(&self) -> Vec<SocketAddr> {
        let mut peers = self.nodes.write();

        // Forget nodes that can't be connected to in case they are offline.
        peers.retain(|_, meta| meta.connection_failures <= MAX_CONNECTION_FAILURE_COUNT);

        let mut addrs = Vec::new();
        for (addr, meta) in peers.iter_mut() {
            // Disconnect from peers we have received the state and sufficient peers from.
            if meta.state.is_some() && meta.received_peer_sets >= DESIRED_PEER_SET_COUNT {
                meta.reset_crawl_state();
                addrs.push(*addr);
            }
        }

        addrs
    }

    /// Returns `true` if the known network contains any connections, `false` otherwise.
    pub fn has_connections(&self) -> bool {
        !self.connections.read().is_empty()
    }

    /// Returns a connection.
    pub fn get_connection(&self, source: SocketAddr, target: SocketAddr) -> Option<Connection> {
        self.connections.read().get(&Connection::new(source, target)).copied()
    }

    /// Returns a snapshot of all the connections.
    pub fn connections(&self) -> HashSet<Connection> {
        self.connections.read().clone()
    }

    /// Returns a snapshot of all the nodes.
    pub fn nodes(&self) -> HashMap<SocketAddr, NodeMeta> {
        self.nodes.read().clone()
    }

    /// Returns a state summary for the known nodes.
    pub fn get_node_summary(&self) -> NetworkSummary {
        let nodes = self.nodes();

        let mut versions = HashMap::with_capacity(nodes.len());
        let mut states = HashMap::with_capacity(nodes.len());
        let mut types = HashMap::with_capacity(nodes.len());
        let mut heights = HashMap::with_capacity(nodes.len());

        let mut handshake_times = Vec::with_capacity(nodes.len());
        let mut nodes_pending_state: usize = 0;

        for meta in nodes.values() {
            if let Some(ref state) = meta.state {
                versions.entry(state.version).and_modify(|count| *count += 1).or_insert(1);
                states.entry(state.state).and_modify(|count| *count += 1).or_insert(1);
                types.entry(state.node_type).and_modify(|count| *count += 1).or_insert(1);
                heights.entry(state.height).and_modify(|count| *count += 1).or_insert(1);
            } else {
                nodes_pending_state += 1;
            }
            if let Some(time) = meta.handshake_time {
                handshake_times.push(time);
            }
        }

        let num_known_connections = self.connections().len();
        let avg_handshake_time_ms = if !handshake_times.is_empty() {
            let avg = handshake_times.iter().sum::<Duration>().whole_milliseconds() as i64 / handshake_times.len() as i64;
            Some(avg)
        } else {
            None
        };

        NetworkSummary {
            num_known_nodes: nodes.len(),
            num_known_connections,
            nodes_pending_state,
            versions,
            heights,
            states,
            types,
            avg_handshake_time_ms,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn connections_update() {
        let addr_a = "11.11.11.11:1000".parse().unwrap();
        let addr_b = "22.22.22.22:2000".parse().unwrap();
        let addr_c = "33.33.33.33:3000".parse().unwrap();
        let addr_d = "44.44.44.44:4000".parse().unwrap();
        let addr_e = "55.55.55.55:5000".parse().unwrap();

        let old_but_valid_timestamp = OffsetDateTime::now_utc() - Duration::hours(STALE_CONNECTION_CUTOFF_TIME_HRS - 1);
        let stale_timestamp = OffsetDateTime::now_utc() - Duration::hours(STALE_CONNECTION_CUTOFF_TIME_HRS + 1);

        // Seed the known network with the older connections.
        let old_but_valid_connection = Connection {
            source: addr_a,
            target: addr_d,
            last_seen: old_but_valid_timestamp,
        };

        let stale_connection = Connection {
            source: addr_a,
            target: addr_e,
            last_seen: stale_timestamp,
        };

        let mut seeded_connections = HashSet::new();
        seeded_connections.insert(old_but_valid_connection);
        seeded_connections.insert(stale_connection);

        let known_network = KnownNetwork {
            nodes: Default::default(),
            connections: RwLock::new(seeded_connections),
        };

        // Insert two connections.
        known_network.update_connections(addr_a, vec![addr_b, addr_c]);
        assert!(known_network.connections.read().contains(&Connection::new(addr_a, addr_b)));
        assert!(known_network.connections.read().contains(&Connection::new(addr_a, addr_c)));
        assert!(known_network.connections.read().contains(&Connection::new(addr_a, addr_d)));
        // Assert the stale connection was purged.
        assert!(!known_network.connections.read().contains(&Connection::new(addr_a, addr_e)));

        // Insert (a, b) connection reversed, make sure it doesn't change the list.
        known_network.update_connections(addr_b, vec![addr_a]);
        assert_eq!(known_network.connections.read().len(), 3);

        // Insert (a, d) again and make sure the timestamp was updated.
        known_network.update_connections(addr_a, vec![addr_d]);
        assert_ne!(
            old_but_valid_timestamp,
            known_network.get_connection(addr_a, addr_d).unwrap().last_seen
        );
    }
}
