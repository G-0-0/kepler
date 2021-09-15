use super::executor::{Executor, JoinHandle};
use behaviour::{GetChannel, NetworkBackendBehaviour, SyncChannel};
use futures::{
    channel::mpsc,
    future, pin_mut,
    stream::{Stream, StreamExt},
    task::AtomicWaker,
};
use libipld::{error::BlockNotFound, store::StoreParams, Cid, Result};
#[cfg(feature = "async_global")]
use libp2p::dns::DnsConfig as Dns;
#[cfg(feature = "async_global")]
use libp2p::tcp::TcpConfig;
use libp2p::{
    core::{
        either::EitherTransport,
        transport::Transport,
        upgrade::{self, SelectUpgrade},
    },
    dns::TokioDnsConfig as Dns,
    identity::PublicKey,
    kad::kbucket::Key as BucketKey,
    mplex::MplexConfig,
    noise::{self, NoiseConfig, X25519Spec},
    pnet::{PnetConfig, PreSharedKey},
    relay::{new_transport_and_behaviour, Relay, RelayConfig},
    swarm::{AddressScore, Swarm, SwarmBuilder},
    tcp::TokioTcpConfig as TcpConfig,
    yamux::YamuxConfig,
};
use parking_lot::Mutex;
use prometheus::Registry;
use std::{
    collections::HashSet,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

mod behaviour;
// mod config;
mod p2p_wrapper;
mod peers;
mod relay;
mod utils;

pub use behaviour::GossipEvent;
use behaviour::{QueryId, SyncEvent};
// use config::*;
use libp2p::{
    core::{connection::ListenerId, identity, transport::memory::MemoryTransport},
    kad::{
        record::{Key, Record},
        PeerRecord, Quorum,
    },
    swarm::AddressRecord,
    Multiaddr, PeerId, TransportError,
};
pub use libp2p_bitswap::BitswapStore;
use libp2p_blake_streams::{DocId, Head, LocalStreamWriter, SignedHead, StreamId, StreamReader};
// pub use libp2p_quic::{generate_keypair, PublicKey, SecretKey, ToLibp2p};
use peers::{AddressSource, Event, PeerInfo, SwarmEvents};
pub use relay::open_relay;

// TODO remove use of rocket, maybe go directly to Tokio
use rocket::tokio::fs;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ListenerEvent {
    NewListenAddr(Multiaddr),
    ExpiredListenAddr(Multiaddr),
}

#[derive(Clone)]
pub struct NetworkService<P: StoreParams> {
    db_path: PathBuf,
    relay_addr: Multiaddr,
    executor: Executor,
    swarm: Arc<Mutex<Swarm<NetworkBackendBehaviour<P>>>>,
    waker: Arc<AtomicWaker>,
    _swarm_task: Arc<JoinHandle<()>>,
}

impl<P: StoreParams> NetworkService<P> {
    pub async fn new<S: BitswapStore<Params = P>>(
        // mut config: NetworkConfig,
        store: S,
        executor: Executor,
        relay_addr: Multiaddr,
        db_path: PathBuf,
    ) -> Result<Self> {
        let kp = if let Ok(mut bytes) = fs::read(db_path.join("kp")).await {
            identity::Keypair::rsa_from_pkcs8(&mut bytes)?
        } else {
            let kp = identity::Keypair::generate_ed25519();
            let bytes = match kp {
                identity::Keypair::Ed25519(ref k) => k.encode(),
                _ => unreachable!(),
            };
            fs::write(db_path.join("kp"), bytes).await?;
            kp
        };

        let transport = TcpConfig::new().nodelay(true).port_reuse(true);
        let (relay_transport, relay_behaviour) =
            new_transport_and_behaviour(RelayConfig::default(), transport);

        let peer_id = kp.public().into_peer_id();
        let behaviour =
            NetworkBackendBehaviour::<P>::new(store, kp.clone(), relay_behaviour).await?;
        // let transport = if let Some(psk) = config.psk {
        //     let psk = PreSharedKey::new(psk);
        //     EitherTransport::Left(
        //         transport.and_then(move |socket, _| PnetConfig::new(psk).handshake(socket)),
        //     )
        // } else {
        //     EitherTransport::Right(transport)
        // };
        let dh_key = noise::Keypair::<X25519Spec>::new()
            .into_authentic(&kp)
            .unwrap();
        let transport = relay_transport
            .upgrade(upgrade::Version::V1)
            .authenticate(NoiseConfig::xx(dh_key).into_authenticated())
            .multiplex(SelectUpgrade::new(
                YamuxConfig::default(),
                MplexConfig::new(),
            ))
            .timeout(Duration::from_secs(5))
            .boxed();
        let tcp = p2p_wrapper::P2pWrapper(transport);

        // #[cfg(feature = "async_global")]
        // let transport = if let Some(config) = config.dns {
        //     Dns::custom(tcp, config.config, config.opts).await?.boxed()
        // } else {
        //     Dns::system(tcp).await?.boxed()
        // };
        // let transport = if let Some(config) = config.dns {
        //     Dns::custom(tcp, config.config, config.opts)?.boxed()
        // } else {
        let transport = Dns::system(tcp)?.boxed();
        // };

        let exec = executor.clone();
        let swarm = SwarmBuilder::new(transport, behaviour, peer_id)
            .executor(Box::new(move |fut| {
                exec.spawn(fut).detach();
            }))
            .build();

        let swarm = Arc::new(Mutex::new(swarm));
        let swarm2 = swarm.clone();
        let waker = Arc::new(AtomicWaker::new());
        let waker2 = waker.clone();
        let swarm_task = executor.spawn(future::poll_fn(move |cx| {
            waker.register(cx.waker());
            let mut guard = swarm.lock();
            while {
                let swarm = &mut *guard;
                pin_mut!(swarm);
                swarm.poll_next(cx).is_ready()
            } {}
            Poll::Pending
        }));

        Ok(Self {
            db_path,
            relay_addr,
            executor,
            swarm: swarm2,
            waker: waker2,
            _swarm_task: Arc::new(swarm_task),
        })
    }

    // pub fn local_public_key(&self) -> PublicKey {
    //     let swarm = self.swarm.lock();
    //     *swarm.behaviour().local_public_key()
    // }

    pub fn local_peer_id(&self) -> PeerId {
        let swarm = self.swarm.lock();
        *swarm.local_peer_id()
    }

    pub fn local_node_name(&self) -> String {
        let swarm = self.swarm.lock();
        swarm.behaviour().local_node_name().to_string()
    }

    pub fn listen_on(&self, addr: Multiaddr) -> Result<impl Stream<Item = ListenerEvent>> {
        let mut swarm = self.swarm.lock();
        let stream = swarm.behaviour_mut().swarm_events();
        let listener = swarm.listen_on(addr)?;
        self.waker.wake();
        Ok(stream
            .take_while(move |event| match event {
                Event::ListenerClosed(id) if *id == listener => future::ready(false),
                _ => future::ready(true),
            })
            .filter_map(move |event| match event {
                Event::NewListenAddr(id, addr) if id == listener => {
                    future::ready(Some(ListenerEvent::NewListenAddr(addr)))
                }
                Event::ExpiredListenAddr(id, addr) if id == listener => {
                    future::ready(Some(ListenerEvent::ExpiredListenAddr(addr)))
                }
                _ => future::ready(None),
            }))
    }

    pub fn listeners(&self) -> Vec<Multiaddr> {
        let swarm = self.swarm.lock();
        swarm.listeners().cloned().collect()
    }

    pub fn add_external_address(&self, mut addr: Multiaddr) {
        peers::normalize_addr(&mut addr, &self.local_peer_id());
        let mut swarm = self.swarm.lock();
        swarm.add_external_address(addr, AddressScore::Infinite);
        self.waker.wake();
    }

    pub fn external_addresses(&self) -> Vec<AddressRecord> {
        let swarm = self.swarm.lock();
        swarm.external_addresses().cloned().collect()
    }

    pub fn add_address(&self, peer: &PeerId, addr: Multiaddr) {
        let mut swarm = self.swarm.lock();
        swarm
            .behaviour_mut()
            .add_address(peer, addr, AddressSource::User);
        self.waker.wake();
    }

    pub fn remove_address(&self, peer: &PeerId, addr: &Multiaddr) {
        let mut swarm = self.swarm.lock();
        swarm.behaviour_mut().remove_address(peer, addr);
        self.waker.wake();
    }

    pub fn dial(&self, peer: &PeerId) {
        let mut swarm = self.swarm.lock();
        swarm.behaviour_mut().dial(peer);
        self.waker.wake();
    }

    pub fn ban(&self, peer: PeerId) {
        let mut swarm = self.swarm.lock();
        swarm.ban_peer_id(peer);
        self.waker.wake();
    }

    pub fn unban(&self, peer: PeerId) {
        let mut swarm = self.swarm.lock();
        swarm.unban_peer_id(peer);
        self.waker.wake();
    }

    pub fn peers(&self) -> Vec<PeerId> {
        let swarm = self.swarm.lock();
        swarm.behaviour().peers().copied().collect()
    }

    pub fn connections(&self) -> Vec<(PeerId, Multiaddr)> {
        let swarm = self.swarm.lock();
        swarm
            .behaviour()
            .connections()
            .map(|(peer_id, addr)| (*peer_id, addr.clone()))
            .collect()
    }

    pub fn is_connected(&self, peer: &PeerId) -> bool {
        let swarm = self.swarm.lock();
        swarm.behaviour().is_connected(peer)
    }

    pub fn peer_info(&self, peer: &PeerId) -> Option<PeerInfo> {
        let swarm = self.swarm.lock();
        swarm.behaviour().info(peer).cloned()
    }

    pub async fn bootstrap(&self, peers: &[(PeerId, Multiaddr)]) -> Result<()> {
        for (peer, addr) in peers {
            self.add_address(peer, addr.clone());
            self.dial(peer);
        }
        let rx = {
            let mut swarm = self.swarm.lock();
            swarm.behaviour_mut().bootstrap()
        };
        tracing::trace!("started bootstrap");
        rx.await??;
        tracing::trace!("boostrap complete");
        Ok(())
    }

    pub fn is_bootstrapped(&self) -> bool {
        let swarm = self.swarm.lock();
        swarm.behaviour().is_bootstrapped()
    }

    pub async fn get_closest_peers<K>(&self, key: K) -> Result<Vec<PeerId>>
    where
        K: Into<BucketKey<K>> + Into<Vec<u8>> + Clone,
    {
        let rx = {
            let mut swarm = self.swarm.lock();
            swarm.behaviour_mut().get_closest_peers(key)
        };
        Ok(rx.await??)
    }

    pub async fn providers(&self, key: Key) -> Result<HashSet<PeerId>> {
        let rx = {
            let mut swarm = self.swarm.lock();
            swarm.behaviour_mut().providers(key)
        };
        Ok(rx.await??)
    }

    pub async fn provide(&self, key: Key) -> Result<()> {
        let rx = {
            let mut swarm = self.swarm.lock();
            swarm.behaviour_mut().provide(key)
        };
        Ok(rx.await??)
    }

    pub fn unprovide(&self, key: &Key) {
        let mut swarm = self.swarm.lock();
        swarm.behaviour_mut().unprovide(key);
        self.waker.wake();
    }

    pub async fn get_record(&self, key: &Key, quorum: Quorum) -> Result<Vec<PeerRecord>> {
        let rx = {
            let mut swarm = self.swarm.lock();
            swarm.behaviour_mut().get_record(key, quorum)
        };
        Ok(rx.await??)
    }

    pub async fn put_record(&self, record: Record, quorum: Quorum) -> Result<()> {
        let rx = {
            let mut swarm = self.swarm.lock();
            swarm.behaviour_mut().put_record(record, quorum)
        };
        rx.await??;
        Ok(())
    }

    pub fn remove_record(&self, key: &Key) {
        let mut swarm = self.swarm.lock();
        swarm.behaviour_mut().remove_record(key);
        self.waker.wake();
    }

    pub fn subscribe(&self, topic: &str) -> Result<impl Stream<Item = GossipEvent>> {
        let mut swarm = self.swarm.lock();
        let stream = swarm.behaviour_mut().subscribe(topic)?;
        self.waker.wake();
        Ok(stream)
    }

    pub fn publish(&self, topic: &str, msg: Vec<u8>) -> Result<()> {
        let mut swarm = self.swarm.lock();
        swarm.behaviour_mut().publish(topic, msg)?;
        self.waker.wake();
        Ok(())
    }

    pub fn broadcast(&self, topic: &str, msg: Vec<u8>) -> Result<()> {
        let mut swarm = self.swarm.lock();
        swarm.behaviour_mut().broadcast(topic, msg)?;
        self.waker.wake();
        Ok(())
    }

    pub fn get(&self, cid: Cid, providers: impl Iterator<Item = PeerId>) -> GetQuery<P> {
        let mut swarm = self.swarm.lock();
        let (rx, id) = swarm.behaviour_mut().get(cid, providers);
        self.waker.wake();
        GetQuery {
            swarm: Some(self.swarm.clone()),
            id,
            rx,
        }
    }

    pub fn sync(&self, cid: Cid, providers: Vec<PeerId>, missing: Vec<Cid>) -> SyncQuery<P> {
        if missing.is_empty() {
            return SyncQuery::ready(Ok(()));
        }
        if providers.is_empty() {
            return SyncQuery::ready(Err(BlockNotFound(missing[0]).into()));
        }
        let mut swarm = self.swarm.lock();
        let (rx, id) = swarm
            .behaviour_mut()
            .sync(cid, providers, missing.into_iter());
        self.waker.wake();
        SyncQuery {
            swarm: Some(self.swarm.clone()),
            id: Some(id),
            rx,
        }
    }

    pub fn register_metrics(&self, registry: &Registry) -> Result<()> {
        let swarm = self.swarm.lock();
        swarm.behaviour().register_metrics(registry)
    }

    pub fn swarm_events(&self) -> SwarmEvents {
        let mut swarm = self.swarm.lock();
        swarm.behaviour_mut().swarm_events()
    }

    pub fn docs(&self) -> Result<Vec<DocId>> {
        let mut swarm = self.swarm.lock();
        swarm.behaviour_mut().streams().docs()
    }

    pub fn streams(&self) -> Result<Vec<StreamId>> {
        let mut swarm = self.swarm.lock();
        swarm.behaviour_mut().streams().streams()
    }

    pub fn substreams(&self, doc: DocId) -> Result<Vec<StreamId>> {
        let mut swarm = self.swarm.lock();
        swarm.behaviour_mut().streams().substreams(doc)
    }

    pub fn stream_add_peers(&self, doc: DocId, peers: impl Iterator<Item = PeerId>) {
        let mut swarm = self.swarm.lock();
        swarm.behaviour_mut().streams().add_peers(doc, peers)
    }

    pub fn stream_head(&self, id: &StreamId) -> Result<Option<SignedHead>> {
        let mut swarm = self.swarm.lock();
        swarm.behaviour_mut().streams().head(id)
    }

    pub fn stream_slice(&self, id: &StreamId, start: u64, len: u64) -> Result<StreamReader> {
        let mut swarm = self.swarm.lock();
        swarm.behaviour_mut().streams().slice(id, start, len)
    }

    pub fn stream_remove(&self, id: &StreamId) -> Result<()> {
        let mut swarm = self.swarm.lock();
        swarm.behaviour_mut().streams().remove(id)
    }

    pub fn stream_append(&self, id: DocId) -> Result<LocalStreamWriter> {
        let mut swarm = self.swarm.lock();
        swarm.behaviour_mut().streams().append(id)
    }

    pub fn stream_subscribe(&self, id: &StreamId) -> Result<()> {
        let mut swarm = self.swarm.lock();
        swarm.behaviour_mut().streams().subscribe(id)
    }

    pub fn stream_update_head(&self, head: SignedHead) {
        let mut swarm = self.swarm.lock();
        swarm.behaviour_mut().streams().update_head(head)
    }
}

pub struct GetQuery<P: StoreParams> {
    swarm: Option<Arc<Mutex<Swarm<NetworkBackendBehaviour<P>>>>>,
    id: QueryId,
    rx: GetChannel,
}

impl<P: StoreParams> Future for GetQuery<P> {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        match Pin::new(&mut self.rx).poll(cx) {
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(err)) => Poll::Ready(Err(err.into())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<P: StoreParams> Drop for GetQuery<P> {
    fn drop(&mut self) {
        let swarm = self.swarm.take().unwrap();
        let mut swarm = swarm.lock();
        swarm.behaviour_mut().cancel(self.id);
    }
}

/// A `bitswap` sync query.
pub struct SyncQuery<P: StoreParams> {
    swarm: Option<Arc<Mutex<Swarm<NetworkBackendBehaviour<P>>>>>,
    id: Option<QueryId>,
    rx: SyncChannel,
}

impl<P: StoreParams> SyncQuery<P> {
    fn ready(res: Result<()>) -> Self {
        let (tx, rx) = mpsc::unbounded();
        tx.unbounded_send(SyncEvent::Complete(res)).unwrap();
        Self {
            swarm: None,
            id: None,
            rx,
        }
    }
}

impl<P: StoreParams> Future for SyncQuery<P> {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        loop {
            match Pin::new(&mut self.rx).poll_next(cx) {
                Poll::Ready(Some(SyncEvent::Complete(result))) => return Poll::Ready(result),
                Poll::Ready(_) => continue,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl<P: StoreParams> Stream for SyncQuery<P> {
    type Item = SyncEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.rx).poll_next(cx)
    }
}

impl<P: StoreParams> Drop for SyncQuery<P> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let swarm = self.swarm.take().unwrap();
            let mut swarm = swarm.lock();
            swarm.behaviour_mut().cancel(id);
        }
    }
}
