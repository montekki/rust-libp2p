// Copyright 2019 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

#![cfg(test)]

use crate::{GetValueResult, Kademlia, KademliaOut, RecordStore, kbucket::{self, Distance}};
use futures::{future, prelude::*};
use libp2p_core::{
    PeerId,
    Swarm,
    Transport,
    identity,
    transport::{MemoryTransport, boxed::Boxed},
    nodes::Substream,
    multiaddr::{Protocol, multiaddr},
    muxing::StreamMuxerBox,
    upgrade,
};
use libp2p_secio::SecioConfig;
use libp2p_yamux as yamux;
use rand::random;
use std::{collections::HashSet, io, u64};
use tokio::runtime::Runtime;
use multihash::{Hash, Multihash};

type TestSwarm = Swarm<
    Boxed<(PeerId, StreamMuxerBox), io::Error>,
    Kademlia<Substream<StreamMuxerBox>>
>;

/// Builds swarms, each listening on a port. Does *not* connect the nodes together.
fn build_nodes(num: usize) -> (u64, Vec<TestSwarm>) {
    let port_base = 1 + random::<u64>() % (u64::MAX - num as u64);
    let mut result: Vec<Swarm<_, _>> = Vec::with_capacity(num);

    for _ in 0 .. num {
        // TODO: make creating the transport more elegant ; literaly half of the code of the test
        //       is about creating the transport
        let local_key = identity::Keypair::generate_ed25519();
        let local_public_key = local_key.public();
        let transport = MemoryTransport::default()
            .with_upgrade(SecioConfig::new(local_key))
            .and_then(move |out, endpoint| {
                let peer_id = out.remote_key.into_peer_id();
                let yamux = yamux::Config::default();
                upgrade::apply(out.stream, yamux, endpoint)
                    .map(|muxer| (peer_id, StreamMuxerBox::new(muxer)))
            })
            .map_err(|e| panic!("Failed to create transport: {:?}", e))
            .boxed();

        let kad = Kademlia::new(local_public_key.clone().into_peer_id());
        result.push(Swarm::new(transport, kad, local_public_key.into_peer_id()));
    }

    let mut i = 0;
    for s in result.iter_mut() {
        Swarm::listen_on(s, Protocol::Memory(port_base + i).into()).unwrap();
        i += 1
    }

    (port_base, result)
}

#[test]
fn query_iter() {
    fn distances(key: &kbucket::Key<PeerId>, peers: Vec<PeerId>) -> Vec<Distance> {
        peers.into_iter()
            .map(kbucket::Key::from)
            .map(|k| k.distance(key))
            .collect()
    }

    fn run(n: usize) {
        // Build `n` nodes. Node `n` knows about node `n-1`, node `n-1` knows about node `n-2`, etc.
        // Node `n` is queried for a random peer and should return nodes `1..n-1` sorted by
        // their distances to that peer.

        let (port_base, mut swarms) = build_nodes(n);
        let swarm_ids: Vec<_> = swarms.iter().map(Swarm::local_peer_id).cloned().collect();

        // Connect each swarm in the list to its predecessor in the list.
        for (i, (swarm, peer)) in &mut swarms.iter_mut().skip(1).zip(swarm_ids.clone()).enumerate() {
            swarm.add_address(&peer, Protocol::Memory(port_base + i as u64).into())
        }

        // Ask the last peer in the list to search a random peer. The search should
        // propagate backwards through the list of peers.
        let search_target = PeerId::random();
        let search_target_key = kbucket::Key::from(search_target.clone());
        swarms.last_mut().unwrap().find_node(search_target.clone());

        // Set up expectations.
        let expected_swarm_id = swarm_ids.last().unwrap().clone();
        let expected_peer_ids: Vec<_> = swarm_ids.iter().cloned().take(n - 1).collect();
        let mut expected_distances = distances(&search_target_key, expected_peer_ids.clone());
        expected_distances.sort();

        // Run test
        Runtime::new().unwrap().block_on(
            future::poll_fn(move || -> Result<_, io::Error> {
                for (i, swarm) in swarms.iter_mut().enumerate() {
                    loop {
                        match swarm.poll().unwrap() {
                            Async::Ready(Some(KademliaOut::FindNodeResult {
                                key, closer_peers
                            })) => {
                                assert_eq!(key, search_target);
                                assert_eq!(swarm_ids[i], expected_swarm_id);
                                assert!(expected_peer_ids.iter().all(|p| closer_peers.contains(p)));
                                let key = kbucket::Key::from(key);
                                assert_eq!(expected_distances, distances(&key, closer_peers));
                                return Ok(Async::Ready(()));
                            }
                            Async::Ready(_) => (),
                            Async::NotReady => break,
                        }
                    }
                }
                Ok(Async::NotReady)
            }))
            .unwrap()
    }

    for n in 2..=8 { run(n) }
}

#[test]
fn unresponsive_not_returned_direct() {
    // Build one node. It contains fake addresses to non-existing nodes. We ask it to find a
    // random peer. We make sure that no fake address is returned.

    let (_, mut swarms) = build_nodes(1);

    // Add fake addresses.
    for _ in 0 .. 10 {
        swarms[0].add_address(&PeerId::random(), Protocol::Udp(10u16).into());
    }

    // Ask first to search a random value.
    let search_target = PeerId::random();
    swarms[0].find_node(search_target.clone());

    Runtime::new().unwrap().block_on(
        future::poll_fn(move || -> Result<_, io::Error> {
            for swarm in &mut swarms {
                loop {
                    match swarm.poll().unwrap() {
                        Async::Ready(Some(KademliaOut::FindNodeResult { key, closer_peers })) => {
                            assert_eq!(key, search_target);
                            assert_eq!(closer_peers.len(), 0);
                            return Ok(Async::Ready(()));
                        }
                        Async::Ready(_) => (),
                        Async::NotReady => break,
                    }
                }
            }

            Ok(Async::NotReady)
        }))
        .unwrap();
}

#[test]
fn unresponsive_not_returned_indirect() {
    // Build two nodes. Node #2 knows about node #1. Node #1 contains fake addresses to
    // non-existing nodes. We ask node #2 to find a random peer. We make sure that no fake address
    // is returned.

    let (port_base, mut swarms) = build_nodes(2);

    // Add fake addresses to first.
    let first_peer_id = Swarm::local_peer_id(&swarms[0]).clone();
    for _ in 0 .. 10 {
        swarms[0].add_address(
            &PeerId::random(),
            multiaddr![Udp(10u16)]
        );
    }

    // Connect second to first.
    swarms[1].add_address(&first_peer_id, Protocol::Memory(port_base).into());

    // Ask second to search a random value.
    let search_target = PeerId::random();
    swarms[1].find_node(search_target.clone());

    Runtime::new().unwrap().block_on(
        future::poll_fn(move || -> Result<_, io::Error> {
            for swarm in &mut swarms {
                loop {
                    match swarm.poll().unwrap() {
                        Async::Ready(Some(KademliaOut::FindNodeResult { key, closer_peers })) => {
                            assert_eq!(key, search_target);
                            assert_eq!(closer_peers.len(), 1);
                            assert_eq!(closer_peers[0], first_peer_id);
                            return Ok(Async::Ready(()));
                        }
                        Async::Ready(_) => (),
                        Async::NotReady => break,
                    }
                }
            }

            Ok(Async::NotReady)
        }))
        .unwrap();
}


#[test]
fn get_value_not_found() {
    let (port_base, mut swarms) = build_nodes(3);

    let swarm_ids: Vec<_> = swarms.iter()
        .map(|swarm| Swarm::local_peer_id(&swarm).clone()).collect();

    swarms[0].add_address(&swarm_ids[1], Protocol::Memory(port_base + 1).into());
    swarms[1].add_address(&swarm_ids[2], Protocol::Memory(port_base + 2).into());

    let target_key = multihash::encode(Hash::SHA2256, &vec![1,2,3]).unwrap();
    swarms[0].get_value(&target_key);

    Runtime::new().unwrap().block_on(
        future::poll_fn(move || -> Result<_, io::Error> {
            for swarm in &mut swarms {
                loop {
                    match swarm.poll().unwrap() {
                        Async::Ready(Some(KademliaOut::GetValueResult(result))) => {
                            if let GetValueResult::NotFound { closest_peers} = result {
                                assert_eq!(closest_peers.len(), 2);
                                assert!(closest_peers.contains(&swarm_ids[1]));
                                assert!(closest_peers.contains(&swarm_ids[2]));
                                return Ok(Async::Ready(()));
                            } else {
                                panic!("Expected GetValueResult::NotFound event");
                            }
                        }
                        Async::Ready(_) => (),
                        Async::NotReady => break,
                    }
                }
            }

            Ok(Async::NotReady)
        }))
        .unwrap()
}

#[test]
fn put_value() {
    let (port_base, mut swarms) = build_nodes(32);

    let swarm_ids: Vec<_> = swarms.iter()
        .map(|swarm| Swarm::local_peer_id(&swarm).clone()).collect();

    // Build the topology, therby avoiding bucket overflow:
    //
    // [0   ..   9] <- 10
    // [10  ..  19] <- 20
    // [20  ..  29] <- 30
    // [10, 20, 30] <- 31 (the publisher of the record)

    // Connect swarms[10] to [0..9]
    for (i, peer) in &mut swarm_ids.iter().take(10).enumerate() {
        swarms[10].add_address(&peer, Protocol::Memory(port_base + i as u64).into());
    }
    // Connect swarms[20] to [10..19]
    for (i, peer) in &mut swarm_ids.iter().enumerate().skip(10).take(10) {
        swarms[20].add_address(&peer, Protocol::Memory(port_base + i as u64).into());
    }
    // Connect swarms[30] to [20..29]
    for (i, peer) in &mut swarm_ids.iter().enumerate().skip(20).take(10) {
        swarms[30].add_address(&peer, Protocol::Memory(port_base + i as u64).into());
    }
    // Connect swarms[31] to 10, 20 and 30, so it gets to know them all.
    swarms[31].add_address(&swarm_ids[10], Protocol::Memory(port_base + 10 as u64).into());
    swarms[31].add_address(&swarm_ids[20], Protocol::Memory(port_base + 20 as u64).into());
    swarms[31].add_address(&swarm_ids[30], Protocol::Memory(port_base + 30 as u64).into());

    let target_key = multihash::encode(Hash::SHA2256, &vec![1,2,3]).unwrap();

    swarms[31].put_value(target_key.clone(), vec![4,5,6]).unwrap();

    struct TestContext {
        target_key: Multihash,
        swarm_ids: Vec<PeerId>,
        swarms: Vec<TestSwarm>,
        have_key: HashSet<PeerId>,
        have_no_key: HashSet<PeerId>
    }

    impl Future for TestContext {
        type Item = ();
        type Error = ();

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            loop {
                let res = self.poll_swarms().unwrap();
                match res {
                    Async::Ready((i, e)) => {
                        match e {
                            KademliaOut::PutValueResult{ .. } => {
                                let (have_key, have_no_key): (Vec<_>, Vec<_>) =
                                    self.swarms.iter().take(31)
                                        .partition(|s| s.records.get(&self.target_key).is_some());

                                assert_eq!(have_key.len(), kbucket::MAX_NODES_PER_BUCKET);
                                assert_eq!(have_no_key.len(), 31 - kbucket::MAX_NODES_PER_BUCKET);

                                let target = kbucket::Key::from(self.target_key.clone());

                                let mut has_distances: Vec<_> = have_key.iter()
                                    .map(|s| s.kbuckets.local_key().clone())
                                    .map(|k| target.distance(&k))
                                    .collect();

                                let mut has_no_distances: Vec<_> = have_no_key.iter()
                                    .map(|s| s.kbuckets.local_key().clone())
                                    .map(|k| target.distance(&k))
                                    .collect();

                                has_distances.sort();
                                has_no_distances.sort();

                                assert!(has_no_distances.first() >= has_distances.last());

                                return Ok(Async::Ready(()));
                            }
                            _ => ()
                        }
                    }
                    Async::NotReady => break,
                }
            }

            Ok(Async::NotReady)
        }
    }

    impl TestContext {
        fn poll_swarms(&mut self) -> Poll<(usize, KademliaOut), ()> {
            for (i, swarm) in self.swarms.iter_mut().enumerate() {
                loop {
                    match swarm.poll().unwrap() {
                        Async::Ready(Some(event)) => {
                            return Ok(Async::Ready((i, event)))
                        },
                        Async::Ready(_) => (),
                        Async::NotReady => break,
                    }
                }
            };

            Ok(Async::NotReady)
        }
    }

    let a = TestContext {
        target_key,
        swarm_ids,
        swarms,
        have_key: Default::default(),
        have_no_key: Default::default(),
    };

    Runtime::new().unwrap().block_on(
        a
    )
    .unwrap();
}

#[test]
fn put_value_many() {
    for _ in 0 .. 1000 { put_value() }
}

