use ethereum_types::H256;
use hmac::{Hmac, Mac, NewMac};
use sha2::Sha256;
use sha3::{Digest, Keccak256};
use std::fmt::{self, Formatter};

pub fn keccak256(data: &[u8]) -> H256 {
    H256::from(Keccak256::digest(data).as_ref())
}

pub fn sha256(data: &[u8]) -> H256 {
    H256::from(Sha256::digest(data).as_ref())
}

pub fn hmac_sha256(key: &[u8], input: &[&[u8]], auth_data: &[u8]) -> H256 {
    let mut hmac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    for input in input {
        hmac.update(input);
    }
    hmac.update(auth_data);
    H256::from_slice(&*hmac.finalize().into_bytes())
}

pub fn hex_debug<T: AsRef<[u8]>>(s: &T, f: &mut Formatter) -> fmt::Result {
    f.write_str(&hex::encode(&s))
}
