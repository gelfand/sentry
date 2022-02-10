use ethereum_types::H256;
use plain_hasher::PlainHasher;
use std::collections::{HashMap, HashSet};

pub type H256Map<T> = HashMap<H256, T, PlainHasher>;
pub type H256Set = HashSet<H256, PlainHasher>;

pub type SentryPeerId = devp2p::PeerIdHash;
pub type P2PPeerId = devp2p::PeerId;

pub fn sentry_peer_id_from_p2p_peer_id(p2p_peer_id: P2PPeerId) -> SentryPeerId {
    devp2p::peer_id::peer_id_hash_from_peer_id(p2p_peer_id)
}
