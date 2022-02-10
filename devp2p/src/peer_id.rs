pub type PeerIdHash = ethereum_types::H256;
pub type PeerIdPubKey = ethereum_types::H512;
pub type PeerId = PeerIdPubKey;
use crate::util::keccak256;
use secp256k1::PublicKey;

pub fn peer_id_from_pub_key(pk: &PublicKey) -> PeerIdPubKey {
    PeerIdPubKey::from_slice(&pk.serialize_uncompressed()[1..])
}

pub fn pub_key_from_peer_id(id: PeerIdPubKey) -> Result<PublicKey, secp256k1::Error> {
    let mut s = [0_u8; 65];
    s[0] = 4;
    s[1..].copy_from_slice(id.as_bytes());
    PublicKey::from_slice(&s)
}

pub fn peer_id_hash_from_peer_id(id: PeerIdPubKey) -> PeerIdHash {
    keccak256(id.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use secp256k1::{SecretKey, SECP256K1};

    #[test]
    fn pk_to_id_pub_key_to_pk() {
        let prikey = SecretKey::new(&mut secp256k1::rand::thread_rng());
        let pubkey = PublicKey::from_secret_key(SECP256K1, &prikey);
        assert_eq!(
            pubkey,
            pub_key_from_peer_id(peer_id_from_pub_key(&pubkey)).unwrap()
        );
    }
}
