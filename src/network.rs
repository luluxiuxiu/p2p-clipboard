use crate::clipboard_content::{
    split_to_network_messages, sha256_hex, ClipboardContent, NetworkMessage,
};
use ed25519_dalek::{pkcs8::DecodePrivateKey, SigningKey};
use futures::prelude::*;
use hex_literal::hex;
use libp2p::gossipsub::{Behaviour, PublishError};
use libp2p::kad::QueryResult;
use libp2p::swarm::ConnectionError;
use libp2p::{
    gossipsub::{self, IdentTopic, MessageAuthenticity, MessageId, ValidationMode},
    identify,
    identity::{Keypair, PublicKey},
    kad::{self, store::MemoryStore},
    multiaddr::Protocol,
    swarm::{self, NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId, Swarm,
};
use libp2p_mdns as mdns;
use libp2p_tls as tls;
use log::{debug, error, info, warn};
use machine_uid;
use std::collections::{hash_map::DefaultHasher, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::{
    error::Error,
    net::{Ipv4Addr, SocketAddrV4},
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{mpsc, oneshot};

/// gossipsub 单条消息最大传输大小（2MB，压缩后）
const MAX_TRANSMIT_SIZE: usize = 2 * 1024 * 1024;
/// 分片传输的超时时间（秒）
const CHUNK_TRANSFER_TIMEOUT_SECS: u64 = 120;

/// 正在进行的分片接收状态
struct ChunkReceiveState {
    total_chunks: u32,
    data_hash: String,
    total_size: u64,
    received: HashMap<u32, Vec<u8>>,
    started_at: Instant,
}

#[derive(NetworkBehaviour)]
struct P2pClipboardBehaviour {
    gossipsub: Behaviour<CompressionTransform>,
    kademlia: kad::Behaviour<MemoryStore>,
    identify: identify::Behaviour,
    mdns: mdns::tokio::Behaviour,
}

#[derive(Clone, Eq, Hash, PartialEq, Debug)]
struct PeerEndpointCache {
    peer_id: PeerId,
    address: Multiaddr,
}

#[derive(Clone, Eq, Hash, PartialEq, Debug)]
struct ConnectionRetryTask {
    target: PeerEndpointCache,
    retry_count: usize,
}

#[derive(Default, Clone)]
struct CompressionTransform;

const ID_SEED: [u8; 118] = hex!("2d2d2d2d2d424547494e2050524956415445204b45592d2d2d2d2d0a4d43344341514177425159444b3256774243494549444c3968565958485271304f48386f774a72363169416a45385a52614263363254373761723564397339670a2d2d2d2d2d454e442050524956415445204b45592d2d2d2d2d");

impl gossipsub::DataTransform for CompressionTransform {
    fn inbound_transform(
        &self,
        raw_message: gossipsub::RawMessage,
    ) -> Result<gossipsub::Message, std::io::Error> {
        let buf: Vec<u8> = zstd::decode_all(&*raw_message.data)?;
        Ok(gossipsub::Message {
            source: raw_message.source,
            data: buf,
            sequence_number: raw_message.sequence_number,
            topic: raw_message.topic,
        })
    }

    fn outbound_transform(
        &self,
        _topic: &gossipsub::TopicHash,
        data: Vec<u8>,
    ) -> Result<Vec<u8>, std::io::Error> {
        let compressed_bytes = zstd::encode_all(&*data, 0)?;
        debug!("Compressed size {}", compressed_bytes.len());
        Ok(compressed_bytes)
    }
}

async fn retry_waiting_thread(
    mut rx: UnboundedReceiver<ConnectionRetryTask>,
    callback: UnboundedSender<PeerEndpointCache>,
    mut shutdown: oneshot::Receiver<()>,
) {
    async fn delayed_callback(
        callback: UnboundedSender<PeerEndpointCache>,
        delay: Duration,
        task: PeerEndpointCache,
    ) {
        tokio::time::sleep(delay).await;
        let _ = callback.send(task);
    }
    loop {
        tokio::select! {
            Some(task) = rx.recv() => {
                let seconds = task.retry_count.checked_pow(2).unwrap_or(0) + 1;
                let payload = task.target;
                tokio::spawn(delayed_callback(callback.clone(), Duration::from_secs(seconds as u64), payload));
            },
            _ = &mut shutdown => {
                debug!("Connection retry waiting thread shutdown received");
                return;
            },
        }
    }
}

pub async fn start_network(
    rx: mpsc::Receiver<ClipboardContent>,
    tx: mpsc::Sender<ClipboardContent>,
    connect_arg: Option<Vec<String>>,
    key_arg: Option<String>,
    listen_arg: Option<String>,
    psk: Option<String>,
    disable_mdns: bool,
) -> Result<(), Box<dyn Error>> {
    let id_keys = match key_arg {
        Some(arg) => {
            let pem = std::fs::read_to_string(arg)?;
            let mut verifying_key_bytes = SigningKey::from_pkcs8_pem(&pem)?.to_bytes();
            Keypair::ed25519_from_bytes(&mut verifying_key_bytes)?
        }
        None => {
            let id: String = machine_uid::get()?;
            let mut key_bytes =
                SigningKey::from_pkcs8_pem(std::str::from_utf8(&ID_SEED)?)?.to_bytes();
            let key = Keypair::ed25519_from_bytes(&mut key_bytes)?;
            let mut new_key = key
                .derive_secret(id.as_ref())
                .expect("can derive secret for ed25519");
            Keypair::ed25519_from_bytes(&mut new_key)?
        }
    };
    let peer_id = PeerId::from(id_keys.public());
    info!("Local peer id: {}", peer_id.to_base58());
    let gossipsub_topic = IdentTopic::new("p2p_clipboard");
    let (boot_addr, boot_peer_id) = match connect_arg {
        Some(arg) => {
            if arg.len() == 2 {
                let peer_id = arg[1].clone().parse::<PeerId>();
                let addr_input = arg[0].clone();
                let sock_addr = parse_ipv4_with_port(Some(addr_input));
                let multiaddr = match sock_addr {
                    Ok((ip, port)) => Ok(format!("/ip4/{}/tcp/{}", ip, port)
                        .parse::<Multiaddr>()
                        .unwrap()),
                    Err(_) => Err(()),
                }
                .unwrap_or_else(|_| {
                    error!("Connect address is not a valid socket address");
                    std::process::exit(1);
                });
                (Some(multiaddr), peer_id.ok())
            } else {
                (None, None)
            }
        }
        None => (None, None),
    };
    let mut swarm: Swarm<P2pClipboardBehaviour> = {
        let mut chat_behaviour = P2pClipboardBehaviour {
            gossipsub: create_gossipsub_behavior(id_keys.clone()),
            kademlia: create_kademlia_behavior(peer_id),
            identify: create_identify_behavior(id_keys.public()),
            mdns: create_mdns_behavior(peer_id, psk.clone(), disable_mdns),
        };
        chat_behaviour
            .gossipsub
            .subscribe(&gossipsub_topic)
            .unwrap();
        libp2p::SwarmBuilder::with_existing_identity(id_keys)
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                tls::Config::new_with_psk(psk),
                yamux::Config::default,
            )?
            .with_behaviour(|_key| chat_behaviour)?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build()
    };
    swarm
        .behaviour_mut()
        .kademlia
        .set_mode(Some(kad::Mode::Server));
    let multiaddr = match listen_arg {
        Some(socket_addr_string) => {
            if let Ok((ip, port)) = parse_ipv4_with_port(Some(socket_addr_string)) {
                Ok(format!("/ip4/{}/tcp/{}", ip, port))
            } else {
                Err(())
            }
        }
        None => Ok("/ip4/0.0.0.0/tcp/0".to_string()),
    }
    .unwrap_or_else(|_| {
        error!("Listen address is not a valid socket address");
        std::process::exit(1);
    });
    let _ = swarm.listen_on(multiaddr.parse()?).unwrap_or_else(|_| {
        error!("Cannot listen on specified address");
        std::process::exit(1);
    });
    let boot_node = {
        if let Some(boot_addr) = boot_addr {
            debug!("Will dial {}", &boot_addr);
            swarm
                .behaviour_mut()
                .kademlia
                .add_address(&boot_peer_id.unwrap(), boot_addr.clone());
            let _ = swarm.dial(boot_addr.clone());
            let _ = swarm.disconnect_peer_id(boot_peer_id.unwrap());
            Some(PeerEndpointCache {
                peer_id: boot_peer_id.unwrap(),
                address: boot_addr.clone(),
            })
        } else {
            None
        }
    };
    let (retry_queue_tx, retry_queue_rx) = mpsc::unbounded_channel::<ConnectionRetryTask>();
    let (retry_callback_queue_tx, retry_callback_queue_rx) =
        mpsc::unbounded_channel::<PeerEndpointCache>();
    let (shutdown_channel_tx, shutdown_channel_rx) = oneshot::channel::<()>();
    let _retry_handle = tokio::spawn(retry_waiting_thread(
        retry_queue_rx,
        retry_callback_queue_tx,
        shutdown_channel_rx,
    ));
    let swarm_handle = tokio::spawn(run(
        swarm,
        gossipsub_topic,
        rx,
        tx,
        boot_node,
        retry_queue_tx,
        retry_callback_queue_rx,
    ));
    swarm_handle.await?;
    let _ = shutdown_channel_tx.send(());
    Ok(())
}

fn handle_incoming_message(
    data: &[u8],
    peer_id: &PeerId,
    id: &MessageId,
    chunk_states: &mut HashMap<String, ChunkReceiveState>,
) -> Option<ClipboardContent> {
    match NetworkMessage::from_bytes(data) {
        Ok(net_msg) => match net_msg {
            NetworkMessage::Direct(content) => {
                debug!("Got direct message: {} with id: {} from peer: {:?}", content.description(), id, peer_id);
                return Some(content);
            }
            NetworkMessage::ChunkStart { transfer_id, total_chunks, data_hash, total_size } => {
                info!("分片传输开始: id={}, chunks={}, size={} bytes, from {:?}", transfer_id, total_chunks, total_size, peer_id);
                chunk_states.insert(transfer_id, ChunkReceiveState {
                    total_chunks, data_hash, total_size,
                    received: HashMap::new(),
                    started_at: Instant::now(),
                });
                None
            }
            NetworkMessage::Chunk { transfer_id, index, data } => {
                if let Some(state) = chunk_states.get_mut(&transfer_id) {
                    debug!("收到分片 {}/{} for transfer {}", index + 1, state.total_chunks, transfer_id);
                    state.received.insert(index, data);
                } else {
                    warn!("收到未知 transfer_id 的分片: {}", transfer_id);
                }
                None
            }
            NetworkMessage::ChunkEnd { transfer_id } => {
                if let Some(state) = chunk_states.remove(&transfer_id) {
                    if state.received.len() as u32 == state.total_chunks {
                        let mut full_data = Vec::with_capacity(state.total_size as usize);
                        for i in 0..state.total_chunks {
                            if let Some(chunk) = state.received.get(&i) {
                                full_data.extend_from_slice(chunk);
                            } else {
                                error!("分片 {} 缺失，传输 {} 失败", i, transfer_id);
                                return None;
                            }
                        }
                        let actual_hash = sha256_hex(&full_data);
                        if actual_hash != state.data_hash {
                            error!("分片传输 {} 哈希校验失败: expected={}, actual={}", transfer_id, state.data_hash, actual_hash);
                            return None;
                        }
                        match ClipboardContent::from_bytes(&full_data) {
                            Ok(content) => {
                                info!("分片传输 {} 完成: {}", transfer_id, content.description());
                                return Some(content);
                            }
                            Err(e) => {
                                error!("分片传输 {} 反序列化失败: {}", transfer_id, e);
                            }
                        }
                    } else {
                        error!("分片传输 {} 不完整: received={}, expected={}", transfer_id, state.received.len(), state.total_chunks);
                    }
                } else {
                    warn!("收到未知 transfer_id 的 ChunkEnd: {}", transfer_id);
                }
                None
            }
        },
        Err(_) => {
            match ClipboardContent::from_bytes(data) {
                Ok(content) => {
                    debug!("Got legacy ClipboardContent: {} with id: {} from peer: {:?}", content.description(), id, peer_id);
                    Some(content)
                }
                Err(_) => {
                    let text = String::from_utf8_lossy(data).to_string();
                    debug!("Got legacy text message with id: {} from peer: {:?}", id, peer_id);
                    Some(ClipboardContent::Text(text))
                }
            }
        }
    }
}

fn cleanup_stale_chunk_states(chunk_states: &mut HashMap<String, ChunkReceiveState>) {
    let timeout = Duration::from_secs(CHUNK_TRANSFER_TIMEOUT_SECS);
    let stale_ids: Vec<String> = chunk_states
        .iter()
        .filter(|(_, state)| state.started_at.elapsed() > timeout)
        .map(|(id, _)| id.clone())
        .collect();
    for id in stale_ids {
        warn!("分片传输 {} 超时，清理状态", id);
        chunk_states.remove(&id);
    }
}

async fn run(
    mut swarm: Swarm<P2pClipboardBehaviour>,
    gossipsub_topic: IdentTopic,
    mut rx: mpsc::Receiver<ClipboardContent>,
    tx: mpsc::Sender<ClipboardContent>,
    boot_node: Option<PeerEndpointCache>,
    retry_queue_tx: UnboundedSender<ConnectionRetryTask>,
    mut retry_callback_queue_rx: UnboundedReceiver<PeerEndpointCache>,
) {
    let mut endpoint_cache: VecDeque<PeerEndpointCache> = VecDeque::new();
    let mut unique_endpoints: HashSet<PeerEndpointCache> = HashSet::new();
    let mut current_listen_addresses: HashSet<Multiaddr> = HashSet::new();
    let mut announced_identities: HashMap<PeerId, Vec<Multiaddr>> = HashMap::new();
    let mut failing_connections: HashMap<PeerEndpointCache, ConnectionRetryTask> = HashMap::new();
    let mut chunk_states: HashMap<String, ChunkReceiveState> = HashMap::new();
    // 待发送的分片消息队列
    let mut pending_chunks: VecDeque<Vec<u8>> = VecDeque::new();
    let mut t = SystemTime::now();
    let mut sleep;
    loop {
        // 每轮循环清理超时的分片状态
        cleanup_stale_chunk_states(&mut chunk_states);
        let to_publish: Option<Vec<u8>> = {
            sleep = Box::pin(tokio::time::sleep(Duration::from_secs(30)).fuse());
            // 优先发送待发送的分片
            if let Some(chunk_data) = pending_chunks.pop_front() {
                Some(chunk_data)
            } else {
            tokio::select! {
                Some(message) = rx.recv() => {
                    debug!("Received local clipboard: {}", message.description());
                    match split_to_network_messages(&message) {
                        Ok(net_msgs) => {
                            if net_msgs.len() == 1 {
                                match net_msgs[0].to_bytes() {
                                    Ok(bytes) => Some(bytes),
                                    Err(e) => { error!("Failed to serialize: {}", e); None }
                                }
                            } else {
                                info!("大数据分片传输: {} 个消息", net_msgs.len());
                                let mut first = None;
                                for msg in net_msgs {
                                    match msg.to_bytes() {
                                        Ok(bytes) => {
                                            if first.is_none() {
                                                first = Some(bytes);
                                            } else {
                                                pending_chunks.push_back(bytes);
                                            }
                                        }
                                        Err(e) => { error!("Failed to serialize chunk: {}", e); }
                                    }
                                }
                                first
                            }
                        }
                        Err(e) => { error!("Failed to split message: {}", e); None }
                    }
                },
                event = swarm.select_next_some() => match event {
                    SwarmEvent::Behaviour(P2pClipboardBehaviourEvent::Gossipsub(ref gossip_event)) => {
                        if let gossipsub::Event::Message {
                            propagation_source: peer_id,
                            message_id: id,
                            message,
                        } = gossip_event
                        {
                            if let Some(content) = handle_incoming_message(&message.data, peer_id, id, &mut chunk_states) {
                                if let Err(e) = tx.send(content).await {
                                    error!("Panic when sending to channel: {}", e);
                                }
                            }
                        }
                        None
                    }
                    SwarmEvent::Behaviour(P2pClipboardBehaviourEvent::Identify(ref identify_event)) => {
                        match identify_event {
                            identify::Event::Received { connection_id: _, peer_id, info: identify::Info { listen_addrs, .. } } => {
                                let old_addrs = announced_identities.insert(*peer_id, listen_addrs.clone());
                                if let Some(old_vec) = old_addrs {
                                    let new: HashSet<Multiaddr> = listen_addrs.iter().cloned().collect();
                                    let old: HashSet<Multiaddr> = old_vec.iter().cloned().collect();
                                    for addr in old.difference(&new) {
                                        debug!("Removing expired addr {addr} trough identify");
                                        swarm.behaviour_mut().kademlia.remove_address(peer_id, addr);
                                    }
                                }
                                for addr in listen_addrs {
                                    debug!("received addr {addr} trough identify");
                                    if !is_multiaddr_local(addr) {
                                        swarm.behaviour_mut().kademlia.add_address(peer_id, addr.clone());
                                    }
                                }
                            }
                            _ => { debug!("got other identify event"); }
                        }
                        None
                    }
                    SwarmEvent::Behaviour(P2pClipboardBehaviourEvent::Kademlia(ref kad_event)) => {
                        match kad_event {
                            kad::Event::RoutingUpdated { peer, .. } => { debug!("Routing updated for {:#?}", peer); },
                            kad::Event::OutboundQueryProgressed { result: QueryResult::GetClosestPeers(result), .. } => {
                                match result {
                                    Ok(kad::GetClosestPeersOk { key: _, peers }) => {
                                        if !peers.is_empty() {
                                            debug!("Query finished with closest peers: {:#?}", peers);
                                        } else {
                                            error!("Query finished with no closest peers.");
                                        }
                                    }
                                    Err(kad::GetClosestPeersError::Timeout { peers, .. }) => {
                                        if !peers.is_empty() {
                                            error!("Query timed out with closest peers: {:#?}", peers);
                                        } else {
                                            error!("Query timed out with no closest peers.");
                                        }
                                    }
                                };
                            }
                            _ => {}
                        }
                        None
                    }
                    SwarmEvent::NewListenAddr { ref address, .. } => {
                        info!("Local node is listening on {address}");
                        let non_local_addr_count = current_listen_addresses.iter().filter(|&addr| !is_multiaddr_local(addr)).count();
                        current_listen_addresses.insert(address.clone());
                        let mut peers_to_push: Vec<PeerId> = Vec::new();
                        if let Some(boot_node_clone) = boot_node.as_ref() {
                            peers_to_push.push(boot_node_clone.peer_id);
                        }
                        swarm.behaviour_mut().identify.push(peers_to_push);
                        let connected_peers_count = swarm.connected_peers().count();
                        debug!("Connected to {connected_peers_count} peers");
                        if boot_node.is_none() {
                            info!("No boot node specified. Waiting for connection.");
                        } else if connected_peers_count == 0 || non_local_addr_count == 0 {
                            if let Some(real_boot_node) = boot_node.as_ref() {
                                let _ = swarm.dial(real_boot_node.address.clone());
                            }
                        }
                        let _ = swarm.behaviour_mut().kademlia.bootstrap();
                        None
                    }
                    SwarmEvent::ExpiredListenAddr { ref address, .. } => {
                        warn!("Local node no longer listening on {address}");
                        current_listen_addresses.remove(address);
                        let non_local_addr_count = current_listen_addresses.iter().filter(|&addr| !is_multiaddr_local(addr)).count();
                        let mut peers_to_push: Vec<PeerId> = Vec::new();
                        if let Some(boot_node_clone) = boot_node.as_ref() {
                            peers_to_push.push(boot_node_clone.peer_id);
                        }
                        swarm.behaviour_mut().identify.push(peers_to_push);
                        if non_local_addr_count > 0 {
                            if let Some(real_boot_node) = boot_node.as_ref() {
                                let retry_task = match failing_connections.get(real_boot_node) {
                                    Some(task) => task.clone(),
                                    None => ConnectionRetryTask { target: real_boot_node.clone(), retry_count: 0 }
                                };
                                failing_connections.insert(real_boot_node.clone(), retry_task.clone());
                                let _ = retry_queue_tx.send(retry_task);
                            }
                        }
                        None
                    }
                    SwarmEvent::ConnectionEstablished { ref peer_id, ref endpoint, .. } => {
                        let real_address = get_non_p2p_multiaddr(endpoint.get_remote_address().clone());
                        let cache = PeerEndpointCache { peer_id: *peer_id, address: real_address.clone() };
                        debug!("Adding endpoint {real_address} to cache");
                        if !unique_endpoints.insert(cache.clone()) {
                            debug!("endpoint {real_address} already in cache, reordering");
                            endpoint_cache.retain(|existing_item| existing_item != &cache);
                        } else {
                            info!("Connected to peer {}", peer_id);
                        }
                        failing_connections.retain(|c, _| c.peer_id != *peer_id);
                        endpoint_cache.push_front(cache);
                        None
                    }
                    SwarmEvent::ConnectionClosed { ref peer_id, ref cause, ref endpoint, ref num_established, .. } => {
                        if *num_established == 0 {
                            warn!("Peer {} has disconnected", peer_id);
                            unique_endpoints.retain(|x| x.peer_id != *peer_id);
                            endpoint_cache.retain(|x| x.peer_id != *peer_id);
                        }
                        if let Some(connection_error) = cause.as_ref() {
                            unique_endpoints.retain(|x| x.address != *endpoint.get_remote_address());
                            endpoint_cache.retain(|x| x.address != *endpoint.get_remote_address());
                            if let ConnectionError::IO(_) = connection_error {
                                let addr = endpoint.get_remote_address();
                                if endpoint.is_dialer() {
                                    let failed_connection = PeerEndpointCache { address: addr.clone(), peer_id: *peer_id };
                                    let retry_task = match failing_connections.get(&failed_connection) {
                                        Some(task) => task.clone(),
                                        None => ConnectionRetryTask { target: failed_connection.clone(), retry_count: 0 }
                                    };
                                    failing_connections.insert(failed_connection, retry_task.clone());
                                    let _ = retry_queue_tx.send(retry_task);
                                }
                            }
                        }
                        None
                    }
                    SwarmEvent::OutgoingConnectionError { ref peer_id, ref error, connection_id } => {
                        debug!("OutgoingConnectionError to {peer_id:?} on {connection_id:?} - {error:?}");
                        let should_clean_peer = match error {
                            swarm::DialError::Transport(errors) => {
                                let mut non_recoverable = false;
                                for (addr, err) in errors {
                                    match err {
                                        libp2p::TransportError::MultiaddrNotSupported(addr) => {
                                            error!("Multiaddr not supported : {addr:?}");
                                            non_recoverable = true;
                                        }
                                        libp2p::TransportError::Other(err) => {
                                            let should_hold_and_retry = ["NetworkUnreachable", "Timeout"];
                                            if let Some(inner) = err.get_ref() {
                                                let error_msg = format!("{inner:?}");
                                                if let Some(peer) = peer_id {
                                                    let fc = PeerEndpointCache { address: addr.clone(), peer_id: *peer };
                                                    if should_hold_and_retry.iter().any(|e| error_msg.contains(e)) {
                                                        let rt = failing_connections.get(&fc).cloned().unwrap_or(ConnectionRetryTask { target: fc.clone(), retry_count: 0 });
                                                        failing_connections.insert(fc, rt.clone());
                                                        let _ = retry_queue_tx.send(rt);
                                                    } else {
                                                        unique_endpoints.retain(|ep| ep.address != *addr);
                                                        endpoint_cache.retain(|ep| ep.address != *addr);
                                                        failing_connections.remove(&fc);
                                                        swarm.behaviour_mut().kademlia.remove_address(peer, addr);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                non_recoverable
                            }
                            swarm::DialError::NoAddresses => { error!("No address provided"); true }
                            swarm::DialError::Aborted => { error!("Aborted"); false }
                            swarm::DialError::DialPeerConditionFalse(_) => { false }
                            swarm::DialError::LocalPeerId { .. } => { error!("Dialing ourselves"); true }
                            swarm::DialError::WrongPeerId { obtained, address } => { error!("WrongPeerId: {obtained:?} {address:?}"); true }
                            swarm::DialError::Denied { cause } => { error!("Denied: {cause:?}"); true }
                        };
                        if should_clean_peer {
                            if let Some(dead_peer) = peer_id {
                                warn!("Cleaning out dead peer {dead_peer:?}");
                                unique_endpoints.retain(|ep| ep.peer_id != *dead_peer);
                                endpoint_cache.retain(|ep| ep.peer_id != *dead_peer);
                                swarm.behaviour_mut().kademlia.remove_peer(dead_peer);
                            }
                        }
                        None
                    }
                    SwarmEvent::Behaviour(P2pClipboardBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                        for (_peer_id, addr) in list {
                            debug!("mDNS discovered a new peer: {_peer_id}");
                            let _ = swarm.dial(addr.clone());
                        }
                        None
                    }
                    SwarmEvent::Behaviour(P2pClipboardBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                        for (peer_id, addr) in list {
                            debug!("mDNS expired a peer: {peer_id}");
                            swarm.behaviour_mut().kademlia.remove_address(&peer_id, &addr);
                        }
                        None
                    }
                    _ => { None }
                },
                Some(failing_connection) = retry_callback_queue_rx.recv() => {
                    if let Some(retry_task) = failing_connections.get_mut(&failing_connection) {
                        let is_network_ok = {
                            let non_local_listener_count = current_listen_addresses.iter().filter(|&addr| !is_multiaddr_local(addr)).count();
                            if swarm.connected_peers().count() > 0 {
                                true
                            } else if non_local_listener_count > 0 {
                                let link_local_count = current_listen_addresses.iter().filter(|&addr| is_multiaddr_link_local(addr)).count();
                                if is_multiaddr_link_local(&retry_task.target.address) {
                                    link_local_count > 0
                                } else {
                                    link_local_count < non_local_listener_count
                                }
                            } else { false }
                        };
                        let is_task_ok = retry_task.retry_count <= 3;
                        let already_connected = swarm.is_connected(&retry_task.target.peer_id);
                        if !is_network_ok {
                            debug!("No working network yet, waiting.");
                            let _ = retry_queue_tx.send(retry_task.clone());
                        } else if !is_task_ok {
                            error!("Connect to {} failed too many times, give up.", retry_task.target.peer_id);
                            failing_connections.remove(&failing_connection);
                        } else if already_connected {
                            debug!("Already connected to {}, stop retrying.", retry_task.target.peer_id);
                            failing_connections.remove(&failing_connection);
                        } else {
                            retry_task.retry_count += 1;
                            let _ = swarm.dial(retry_task.target.address.clone());
                        }
                    }
                    None
                },
                _ = &mut sleep => {
                    debug!("Long idle detected, doing periodic jobs");
                    let stale_peers: Vec<_> = swarm.behaviour_mut().gossipsub.all_peers()
                        .filter(|(_, topics)| topics.is_empty())
                        .map(|(peer, _)| peer.clone())
                        .collect();
                    for peer in stale_peers {
                        let _ = swarm.disconnect_peer_id(peer);
                    }
                    if let Some(boot) = boot_node.as_ref() {
                        if swarm.connected_peers().count() < 1 {
                            let _ = swarm.dial(boot.address.clone());
                        }
                    }
                    let self_id = *swarm.local_peer_id();
                    swarm.behaviour_mut().kademlia.get_closest_peers(self_id);
                    None
                }
            }
            }
        };
        let d = SystemTime::now()
            .duration_since(t)
            .unwrap_or_else(|_| Duration::from_secs(0));
        t = SystemTime::now();
        if d > Duration::from_secs(60) {
            warn!("Handler completed longer than expected, restarting swarm");
            return;
        }
        if let Some(data) = to_publish {
            if let Err(err) = swarm
                .behaviour_mut()
                .gossipsub
                .publish(gossipsub_topic.clone(), data)
            {
                match err {
                    PublishError::Duplicate => {}
                    _ => { error!("Error publishing message: {}", err); }
                }
            }
        }
    }
}

fn create_gossipsub_behavior(id_keys: Keypair) -> Behaviour<CompressionTransform> {
    let message_id_fn = |message: &gossipsub::Message| {
        let mut s = DefaultHasher::new();
        message.data.hash(&mut s);
        MessageId::from(s.finish().to_string())
    };
    let gossipsub_config = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_secs(10))
        .validation_mode(ValidationMode::Strict)
        .message_id_fn(message_id_fn)
        .max_transmit_size(MAX_TRANSMIT_SIZE)
        .do_px()
        .build()
        .expect("Valid config");
    Behaviour::new_with_transform(
        MessageAuthenticity::Signed(id_keys),
        gossipsub_config,
        CompressionTransform,
    )
    .expect("Correct configuration")
}

fn create_kademlia_behavior(local_peer_id: PeerId) -> kad::Behaviour<MemoryStore> {
    let mut cfg = kad::Config::default();
    cfg.set_query_timeout(Duration::from_secs(5 * 60));
    let store = MemoryStore::new(local_peer_id);
    kad::Behaviour::with_config(local_peer_id, store, cfg)
}

fn create_identify_behavior(local_public_key: PublicKey) -> identify::Behaviour {
    identify::Behaviour::new(identify::Config::new(
        "/p2pclipboard/1.0.0".into(),
        local_public_key,
    ))
}

fn create_mdns_behavior(
    local_peer_id: PeerId,
    pre_shared_key: Option<String>,
    disable_mdns: bool,
) -> mdns::Behaviour<mdns::tokio::Tokio> {
    let mut mdns_config = mdns::Config::default();
    let fingerprint = match pre_shared_key {
        Some(psk) => {
            let mut seed_key_bytes =
                SigningKey::from_pkcs8_pem(std::str::from_utf8(&ID_SEED).unwrap())
                    .unwrap()
                    .to_bytes();
            let seed_key = Keypair::ed25519_from_bytes(&mut seed_key_bytes).unwrap();
            Some(Vec::from(
                seed_key
                    .derive_secret(psk.as_ref())
                    .expect("can derive secret for ed25519"),
            ))
        }
        None => None,
    };
    mdns_config.service_fingerprint = fingerprint;
    mdns_config.disabled = disable_mdns;
    mdns::tokio::Behaviour::new(mdns_config, local_peer_id).expect("mdns correct")
}

fn parse_ipv4_with_port(input: Option<String>) -> Result<(Ipv4Addr, u16), &'static str> {
    if let Some(input_str) = input {
        let parts: Vec<&str> = input_str.split(':').collect();
        if parts.len() == 2 {
            let socket_address: Result<SocketAddrV4, _> = input_str.parse();
            match socket_address {
                Ok(socket) => Ok((*socket.ip(), socket.port())),
                Err(_) => Err("Invalid input format"),
            }
        } else if parts.len() == 1 {
            let ip_addr: Result<Ipv4Addr, _> = parts[0].parse();
            match ip_addr {
                Ok(ip) => Ok((ip, 0)),
                _ => Err("Invalid IP address or port number"),
            }
        } else {
            Err("Invalid input format")
        }
    } else {
        Err("Input is None")
    }
}

fn get_non_p2p_multiaddr(mut origin_addr: Multiaddr) -> Multiaddr {
    while origin_addr.iter().count() > 2 {
        let _ = origin_addr.pop();
    }
    origin_addr
}

fn is_multiaddr_link_local(addr: &Multiaddr) -> bool {
    if let Protocol::Ip4(ip) = addr.iter().collect::<Vec<_>>()[0] {
        return ip.is_link_local();
    }
    false
}

fn is_multiaddr_local(addr: &Multiaddr) -> bool {
    addr.iter().collect::<Vec<_>>()[0] == Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1))
}
