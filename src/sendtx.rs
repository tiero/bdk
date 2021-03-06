/*
 * Copyright 2019 Tamas Blummer
 * Copyright 2020 BDK Team
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */
use std::{
    collections::HashMap,
    sync::mpsc,
    thread,
    time::SystemTime
};

use bitcoin::network::message::NetworkMessage;
use bitcoin::network::message_blockdata::{Inventory, InvType};
use bitcoin::Transaction;
use bitcoin_hashes::sha256d;
use log::debug;
use lru_cache::LruCache;
use murmel::p2p::{P2PControlSender, PeerMessage, PeerMessageReceiver, PeerMessageSender};

use crate::db::SharedDB;

pub struct SendTx {
    p2p: P2PControlSender<NetworkMessage>,
    db: SharedDB,
    cache: LruCache<sha256d::Hash, Transaction>
}

const CACHE_SIZE: usize=1000;

impl SendTx {
    pub fn new(p2p: P2PControlSender<NetworkMessage>, db: SharedDB) -> PeerMessageSender<NetworkMessage> {
        let (sender, receiver) = mpsc::sync_channel(p2p.back_pressure);

        let mut own_unconfirmed = HashMap::new();
        {
            let mut db = db.lock().unwrap();
            let tx = db.transaction();
            for (t, _) in tx.read_unconfirmed().expect("can not read unconfirmed transactions") {
                own_unconfirmed.insert(t.txid(), t);
            }
        }

        let mut txsender = SendTx { p2p, db, cache: LruCache::new(CACHE_SIZE) };

        thread::Builder::new().name("sendtx".to_string()).spawn(move || { txsender.run(receiver) }).unwrap();

        PeerMessageSender::new(sender)
    }

    fn run(&mut self, receiver: PeerMessageReceiver<NetworkMessage>) {
        let mut last_announcement = SystemTime::now();
        while let Ok(msg) = receiver.recv() {
            match msg {
                PeerMessage::Incoming(pid, msg) => {
                    match msg {
                        NetworkMessage::GetData(ref inv) => {
                            let txs = inv.iter().filter_map(|i| if i.inv_type == InvType::Transaction { Some(i.hash) } else { None }).collect::<Vec<_>>();
                            if !txs.is_empty() {
                                let txs = txs.iter().filter_map(|h| {
                                    if let Some(cached) = self.cache.get_mut(h) {
                                        self.p2p.send_network(pid, NetworkMessage::Tx(cached.clone()));
                                        None
                                    } else {
                                        Some(*h)
                                    }
                                }).collect::<Vec<_>>();

                                if !txs.is_empty() {
                                    let mut db = self.db.lock().unwrap();
                                    let tx = db.transaction();
                                    for (t, _) in tx.read_unconfirmed().expect("can not read unconfirmed transactions").iter().filter(|(t, _)| txs.contains(&t.txid())) {
                                        self.p2p.send_network(pid, NetworkMessage::Tx(t.clone()));
                                        debug!("sent our transaction {} at request of peer={}", t.txid(), pid);
                                    }
                                }
                            }
                        }
                        NetworkMessage::Inv(ref inv) => {
                            let have_not = inv.iter().filter(|i| i.inv_type == InvType::Transaction && !self.cache.contains_key(&i.hash)).cloned().collect::<Vec<_>>();
                            if !have_not.is_empty() {
                                self.p2p.send_network(pid, NetworkMessage::GetData(have_not));
                            }
                        }
                        NetworkMessage::Tx(ref tx) => {
                            if self.cache.insert(tx.txid(), tx.clone()).is_none() {
                                self.p2p.send_random_network(NetworkMessage::Inv(vec!(Inventory { inv_type: InvType::Transaction, hash: tx.txid() })));
                            }
                        }
                        _ => {}
                    }
                },
                PeerMessage::Outgoing(msg) => {
                    match msg {
                        NetworkMessage::Tx(ref transaction) => {
                            let txid = transaction.txid();
                            self.p2p.send_random_network(NetworkMessage::Inv(vec!(Inventory { hash: txid, inv_type: InvType::Transaction })));
                        },
                        _ => {}
                    }
                }
                _ => {}
            }
            if SystemTime::now().duration_since(last_announcement).unwrap().as_secs() > 60 {
                let mut db = self.db.lock().unwrap();
                let tx = db.transaction();
                for (transaction, _) in tx.read_unconfirmed().expect("can not read unconfirmed transactions") {
                    if !self.cache.contains_key(&transaction.txid()) {
                        if let Some(peer) = self.p2p.send_random_network(NetworkMessage::Inv(vec!(Inventory { hash: transaction.txid(), inv_type: InvType::Transaction }))) {
                            debug!("announced our transaction {} to peer={}", transaction.txid(), peer);
                        }
                    }
                }
                last_announcement = SystemTime::now();
            }
        }
    }
}