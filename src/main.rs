#![allow(dead_code, clippy::upper_case_acronyms)]

use crate::types::sentry_peer_id_from_p2p_peer_id;
use crate::types::P2PPeerId;
use crate::types::SentryPeerId;
use crate::{config::*, eth::*, services::*};
use anyhow::{anyhow, Context};
use async_stream::stream;
use async_trait::async_trait;
use clap::Parser;
use devp2p::*;
use educe::Educe;
use ethereum_interfaces::sentry::{self, sentry_server::SentryServer, InboundMessage, PeersReply};
use futures::stream::BoxStream;
use maplit::btreemap;
use num_traits::{FromPrimitive, ToPrimitive};
use parking_lot::RwLock;
use secp256k1::{PublicKey, SecretKey, SECP256K1};
use std::{
    collections::{btree_map::Entry, hash_map::Entry as HashMapEntry, BTreeMap, HashMap, HashSet},
    fmt::Debug,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use task_group::TaskGroup;
use tokio::{
    sync::{
        broadcast::{channel as broadcast, Sender as BroadcastSender},
        mpsc::{channel, Sender},
        Mutex as AsyncMutex,
    },
    time::sleep,
};
use tokio_stream::{StreamExt, StreamMap};
use tonic::transport::Server;
use tracing::*;
use tracing_subscriber::{prelude::*, EnvFilter};
use trust_dns_resolver::{config::*, TokioAsyncResolver};

mod config;
mod eth;
mod grpc;
mod services;
mod types;

type OutboundSender = Sender<OutboundEvent>;
type OutboundReceiver = Arc<AsyncMutex<BoxStream<'static, OutboundEvent>>>;

pub const BUFFERING_FACTOR: usize = 5;

#[derive(Clone)]
struct Pipes {
    sender: OutboundSender,
    receiver: OutboundReceiver,
}

#[derive(Clone, Debug, Default)]
struct BlockTracker {
    block_by_peer: HashMap<SentryPeerId, u64>,
    peers_by_block: BTreeMap<u64, HashSet<SentryPeerId>>,
}

impl BlockTracker {
    fn set_block_number(&mut self, peer: SentryPeerId, block: u64, force_create: bool) {
        match self.block_by_peer.entry(peer) {
            HashMapEntry::Vacant(e) => {
                if force_create {
                    e.insert(block);
                } else {
                    return;
                }
            }
            HashMapEntry::Occupied(mut e) => {
                let old_block = std::mem::replace(e.get_mut(), block);
                if let Entry::Occupied(mut entry) = self.peers_by_block.entry(old_block) {
                    entry.get_mut().remove(&peer);

                    if entry.get().is_empty() {
                        entry.remove();
                    }
                }
            }
        }

        self.peers_by_block.entry(block).or_default().insert(peer);
    }

    fn remove_peer(&mut self, peer: SentryPeerId) {
        if let Some(block) = self.block_by_peer.remove(&peer) {
            if let Entry::Occupied(mut entry) = self.peers_by_block.entry(block) {
                entry.get_mut().remove(&peer);

                if entry.get().is_empty() {
                    entry.remove();
                }
            }
        }
    }

    fn peers_with_min_block(&self, block: u64) -> HashSet<SentryPeerId> {
        self.peers_by_block
            .range(block..)
            .map(|(_, v)| v)
            .flatten()
            .copied()
            .collect()
    }
}

#[derive(Educe)]
#[educe(Debug)]
pub struct CapabilityServerImpl {
    #[educe(Debug(ignore))]
    peer_pipes: Arc<RwLock<HashMap<SentryPeerId, Pipes>>>,
    block_tracker: Arc<RwLock<BlockTracker>>,

    status_message: Arc<RwLock<Option<FullStatusData>>>,
    protocol_version: EthProtocolVersion,
    valid_peers: Arc<RwLock<HashSet<SentryPeerId>>>,

    data_sender: BroadcastSender<InboundMessage>,
    peers_status_sender: BroadcastSender<PeersReply>,

    no_new_peers: Arc<AtomicBool>,
}

impl CapabilityServerImpl {
    fn setup_peer(&self, peer: SentryPeerId, p: Pipes) {
        let mut pipes = self.peer_pipes.write();
        let mut block_tracker = self.block_tracker.write();

        assert!(pipes.insert(peer, p).is_none());
        block_tracker.set_block_number(peer, 0, true);
    }

    fn get_pipes(&self, peer: SentryPeerId) -> Option<Pipes> {
        self.peer_pipes.read().get(&peer).cloned()
    }

    pub fn sender(&self, peer: SentryPeerId) -> Option<OutboundSender> {
        self.peer_pipes
            .read()
            .get(&peer)
            .map(|pipes| pipes.sender.clone())
    }

    fn receiver(&self, peer: SentryPeerId) -> Option<OutboundReceiver> {
        self.peer_pipes
            .read()
            .get(&peer)
            .map(|pipes| pipes.receiver.clone())
    }

    #[instrument(name = "CapabilityServerImpl.teardown_peer", skip(self))]
    fn teardown_peer(&self, peer: SentryPeerId) {
        let mut pipes = self.peer_pipes.write();
        let mut block_tracker = self.block_tracker.write();
        let mut valid_peers = self.valid_peers.write();

        pipes.remove(&peer);
        block_tracker.remove_peer(peer);
        valid_peers.remove(&peer);

        let send_status_result =
            self.peers_status_sender
                .send(ethereum_interfaces::sentry::PeersReply {
                    peer_id: Some(peer.into()),
                    event: ethereum_interfaces::sentry::peers_reply::PeerEvent::Disconnect as i32,
                });
        if send_status_result.is_err() {
            debug!("No subscribers to report peer status to");
        }
    }

    pub fn all_peers(&self) -> HashSet<SentryPeerId> {
        self.peer_pipes.read().keys().copied().collect()
    }

    pub fn connected_peers(&self) -> usize {
        self.valid_peers.read().len()
    }

    pub fn set_status(&self, message: FullStatusData) {
        *self.status_message.write() = Some(message);
        self.no_new_peers.store(false, Ordering::SeqCst);
    }

    #[instrument(name = "CapabilityServerImpl.handle_event", skip(self, event))]
    fn handle_event(
        &self,
        peer: SentryPeerId,
        event: InboundEvent,
    ) -> Result<Option<Message>, DisconnectReason> {
        match event {
            InboundEvent::Disconnect { reason } => {
                debug!("Peer disconnect (reason: {:?}), tearing down peer.", reason);
                self.teardown_peer(peer);
            }
            InboundEvent::Message {
                message: Message { id, data },
                ..
            } => {
                let valid_peer = self.valid_peers.read().contains(&peer);
                let message_id = EthMessageId::from_usize(id);
                match message_id {
                    None => {
                        debug!("Unknown message");
                    }
                    Some(EthMessageId::Status) => {
                        let v = rlp::decode::<StatusMessage>(&data).map_err(|e| {
                            debug!("Failed to decode status message: {}! Kicking peer.", e);

                            DisconnectReason::ProtocolBreach
                        })?;

                        debug!("Decoded status message: {:?}", v);

                        let status_data = self.status_message.read();
                        let mut valid_peers = self.valid_peers.write();
                        if let Some(FullStatusData { fork_filter, .. }) = &*status_data {
                            fork_filter.validate(v.fork_id).map_err(|reason| {
                                debug!("Kicking peer with incompatible fork ID: {:?}", reason);

                                DisconnectReason::UselessPeer
                            })?;

                            valid_peers.insert(peer);

                            let send_status_result =
                                self.peers_status_sender
                                    .send(ethereum_interfaces::sentry::PeersReply {
                                    peer_id: Some(peer.into()),
                                    event:
                                        ethereum_interfaces::sentry::peers_reply::PeerEvent::Connect
                                            as i32,
                                });
                            if send_status_result.is_err() {
                                debug!("No subscribers to report peer status to");
                            }
                        }
                    }
                    Some(inbound_id) if valid_peer => {
                        if self
                            .data_sender
                            .send(InboundMessage {
                                id: sentry::MessageId::from(inbound_id) as i32,
                                data,
                                peer_id: Some(peer.into()),
                            })
                            .is_err()
                        {
                            warn!("no connected sentry, dropping status and peer");
                            *self.status_message.write() = None;

                            return Err(DisconnectReason::ClientQuitting);
                        }
                    }
                    _ => {}
                }
            }
        }

        Ok(None)
    }
}

#[async_trait]
impl CapabilityServer for CapabilityServerImpl {
    #[instrument(skip(self, p2p_peer_id), level = "debug", fields(peer=&*p2p_peer_id.to_string()))]
    fn on_peer_connect(
        &self,
        p2p_peer_id: P2PPeerId,
        caps: HashMap<CapabilityName, CapabilityVersion>,
    ) {
        let peer: SentryPeerId = sentry_peer_id_from_p2p_peer_id(p2p_peer_id);
        let first_events = if let Some(FullStatusData {
            status,
            fork_filter,
        }) = &*self.status_message.read()
        {
            let status_message = StatusMessage {
                protocol_version: *caps
                    .get(&capability_name())
                    .expect("peer without this cap would have been disconnected"),
                network_id: status.network_id,
                total_difficulty: status.total_difficulty,
                best_hash: status.best_hash,
                genesis_hash: status.fork_data.genesis,
                fork_id: fork_filter.current(),
            };

            vec![OutboundEvent::Message {
                capability_name: capability_name(),
                message: Message {
                    id: EthMessageId::Status.to_usize().unwrap(),
                    data: rlp::encode(&status_message).into(),
                },
            }]
        } else {
            vec![OutboundEvent::Disconnect {
                reason: DisconnectReason::DisconnectRequested,
            }]
        };

        let (sender, mut receiver) = channel(1);
        self.setup_peer(
            peer,
            Pipes {
                sender,
                receiver: Arc::new(AsyncMutex::new(Box::pin(stream! {
                    for event in first_events {
                        yield event;
                    }

                    while let Some(event) = receiver.recv().await {
                        yield event;
                    }
                }))),
            },
        );
    }

    #[instrument(skip_all, level = "debug", fields(peer=&*p2p_peer_id.to_string(), event=&*event.to_string()))]
    async fn on_peer_event(&self, p2p_peer_id: P2PPeerId, event: InboundEvent) {
        debug!("Received message");
        let peer: SentryPeerId = sentry_peer_id_from_p2p_peer_id(p2p_peer_id);

        if let Some(ev) = self.handle_event(peer, event).transpose() {
            let _ = self
                .sender(peer)
                .unwrap()
                .send(match ev {
                    Ok(message) => OutboundEvent::Message {
                        capability_name: capability_name(),
                        message,
                    },
                    Err(reason) => OutboundEvent::Disconnect { reason },
                })
                .await;
        }
    }

    async fn next(&self, p2p_peer_id: P2PPeerId) -> OutboundEvent {
        let peer: SentryPeerId = sentry_peer_id_from_p2p_peer_id(p2p_peer_id);
        self.receiver(peer)
            .unwrap()
            .lock()
            .await
            .next()
            .await
            .unwrap_or(OutboundEvent::Disconnect {
                reason: DisconnectReason::DisconnectRequested,
            })
    }
}

struct OptsDnsDisc {
    address: String,
}

impl OptsDnsDisc {
    fn make_task(self) -> anyhow::Result<DnsDiscovery> {
        info!("Starting DNS discovery fetch from {}", self.address);

        let dns_resolver = dnsdisc::Resolver::new(Arc::new(
            TokioAsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default())
                .context("Failed to start DNS resolver")?,
        ));

        let task = DnsDiscovery::new(Arc::new(dns_resolver), self.address, None);

        Ok(task)
    }
}

struct OptsDiscV4 {
    discv4_port: u16,
    discv4_bootnodes: Vec<Discv4NR>,
    discv4_cache: usize,
    discv4_concurrent_lookups: usize,
    listen_port: u16,
}

impl OptsDiscV4 {
    async fn make_task(self, secret_key: &SecretKey) -> anyhow::Result<Discv4> {
        info!("Starting discv4 at port {}", self.discv4_port);

        let mut bootstrap_nodes = self
            .discv4_bootnodes
            .into_iter()
            .map(|Discv4NR(nr)| nr)
            .collect::<Vec<_>>();

        if bootstrap_nodes.is_empty() {
            bootstrap_nodes = BOOTNODES
                .iter()
                .map(|b| Ok(Discv4NR::from_str(b)?.0))
                .collect::<Result<Vec<_>, <Discv4NR as FromStr>::Err>>()?;
            info!("Using default discv4 bootstrap nodes");
        }

        let node = discv4::Node::new(
            format!("0.0.0.0:{}", self.discv4_port).parse().unwrap(),
            *secret_key,
            bootstrap_nodes,
            None,
            true,
            self.listen_port,
        )
        .await?;

        let task = Discv4Builder::default()
            .with_cache(self.discv4_cache)
            .with_concurrent_lookups(self.discv4_concurrent_lookups)
            .build(node);

        Ok(task)
    }
}

struct OptsDiscV5 {
    discv5_enr: Option<discv5::Enr>,
    discv5_addr: Option<String>,
    discv5_bootnodes: Vec<discv5::Enr>,
}

impl OptsDiscV5 {
    async fn make_task(self, secret_key: &SecretKey) -> anyhow::Result<Discv5> {
        let addr = self
            .discv5_addr
            .ok_or_else(|| anyhow!("no discv5 addr specified"))?;
        let enr = self
            .discv5_enr
            .ok_or_else(|| anyhow!("discv5 ENR not specified"))?;

        let mut svc = discv5::Discv5::new(
            enr,
            discv5::enr::CombinedKey::Secp256k1(
                k256::ecdsa::SigningKey::from_bytes(secret_key.as_ref()).unwrap(),
            ),
            Default::default(),
        )
        .map_err(|e| anyhow!("{}", e))?;

        svc.start(addr.parse()?)
            .await
            .map_err(|e| anyhow!("{}", e))
            .context("Failed to start discv5")?;

        info!("Starting discv5 at {}", addr);

        for bootnode in self.discv5_bootnodes {
            svc.add_enr(bootnode).unwrap();
        }

        let task = Discv5::new(svc, 20);
        Ok(task)
    }
}

struct OptsDiscStatic {
    static_peers: Vec<NR>,
    static_peers_interval: u64,
}

impl OptsDiscStatic {
    fn make_task(self) -> anyhow::Result<StaticNodes> {
        info!("Enabling static peers: {:?}", self.static_peers);

        let task = StaticNodes::new(
            self.static_peers
                .iter()
                .map(|&NR(NodeRecord { addr, id })| (addr, id))
                .collect::<HashMap<_, _>>(),
            Duration::from_millis(self.static_peers_interval),
        );
        Ok(task)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts: Opts = Opts::parse();
    fdlimit::raise_fd_limit();

    let filter = if std::env::var(EnvFilter::DEFAULT_ENV)
        .unwrap_or_default()
        .is_empty()
    {
        EnvFilter::new("ethereum_sentry=info,devp2p=info,discv4=info,discv5=info,dnsdisc=info")
    } else {
        EnvFilter::from_default_env()
    };
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .init();

    let secret_key;
    if let Some(data) = opts.node_key {
        secret_key = SecretKey::from_slice(&hex::decode(data)?)?;
        info!("Loaded node key from config");
    } else {
        secret_key = SecretKey::new(&mut secp256k1::rand::thread_rng());
        info!("Generated new node key: {}", secret_key);
    };

    let listen_addr = format!("0.0.0.0:{}", opts.listen_port);

    info!("Starting Ethereum sentry");

    info!(
        "Node ID: {}",
        hex::encode(
            devp2p::peer_id::peer_id_from_pub_key(&PublicKey::from_secret_key(
                SECP256K1,
                &secret_key
            ))
            .as_bytes()
        )
    );

    if let Some(cidr_filter) = &opts.cidr {
        info!("Peers restricted to range {}", cidr_filter);
    }

    let mut discovery_tasks: StreamMap<String, Discovery> = StreamMap::new();

    if !opts.no_discovery {
        let task_opts = OptsDnsDisc {
            address: opts.dnsdisc_address,
        };
        let task = task_opts.make_task()?;
        discovery_tasks.insert("dnsdisc".to_string(), Box::pin(task));

        let task_opts = OptsDiscV4 {
            discv4_port: opts.discv4_port,
            discv4_bootnodes: opts.discv4_bootnodes,
            discv4_cache: opts.discv4_cache,
            discv4_concurrent_lookups: opts.discv4_concurrent_lookups,
            listen_port: opts.listen_port,
        };
        let task = task_opts.make_task(&secret_key).await?;
        discovery_tasks.insert("discv4".to_string(), Box::pin(task));

        if opts.discv5 {
            let task_opts = OptsDiscV5 {
                discv5_enr: opts.discv5_enr,
                discv5_addr: opts.discv5_addr,
                discv5_bootnodes: opts.discv5_bootnodes,
            };
            let task = task_opts.make_task(&secret_key).await?;
            discovery_tasks.insert("discv5".to_string(), Box::pin(task));
        }
    }

    if !opts.static_peers.is_empty() {
        let task_opts = OptsDiscStatic {
            static_peers: opts.static_peers,
            static_peers_interval: opts.static_peers_interval,
        };
        let task = task_opts.make_task()?;
        discovery_tasks.insert("static peers".to_string(), Box::pin(task));
    }

    if discovery_tasks.is_empty() {
        warn!("All discovery methods are disabled, sentry will not search for peers.");
    }

    let tasks = Arc::new(TaskGroup::new());

    let protocol_version = EthProtocolVersion::Eth66;
    let data_sender = broadcast(opts.max_peers * BUFFERING_FACTOR).0;
    let peers_status_sender = broadcast(opts.max_peers).0;
    let no_new_peers = Arc::new(AtomicBool::new(true));

    let capability_server = Arc::new(CapabilityServerImpl {
        peer_pipes: Default::default(),
        block_tracker: Default::default(),
        status_message: Default::default(),
        protocol_version,
        valid_peers: Default::default(),
        data_sender,
        peers_status_sender,
        no_new_peers: no_new_peers.clone(),
    });

    let swarm = Swarm::builder()
        .with_task_group(tasks.clone())
        .with_listen_options(ListenOptions {
            discovery_tasks,
            max_peers: opts.max_peers,
            addr: listen_addr.parse().unwrap(),
            cidr: opts.cidr,
            no_new_peers,
        })
        .with_client_version(format!("sentry/v{}", env!("CARGO_PKG_VERSION")))
        .build(
            btreemap! {
                CapabilityId { name: capability_name(), version: protocol_version as CapabilityVersion } => 17,
            },
            capability_server.clone(),
            secret_key,
        )
        .await
        .context("Failed to start RLPx node")?;

    info!("RLPx node listening at {}", listen_addr);

    let sentry_addr = opts.sentry_addr.parse()?;
    tasks.spawn(async move {
        let svc = SentryServer::new(SentryService::new(capability_server));

        info!("Sentry gRPC server starting on {}", sentry_addr);

        Server::builder()
            .add_service(svc)
            .serve(sentry_addr)
            .await
            .unwrap();
    });

    loop {
        info!(
            "Peer info: {} active (+{} dialing) / {} max.",
            swarm.connected_peers(),
            swarm.dialing(),
            opts.max_peers
        );

        sleep(Duration::from_secs(5)).await;
    }
}
