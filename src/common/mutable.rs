//! Helper functions and structs for mutable items.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha1_smol::Sha1;
use std::convert::TryFrom;

use crate::Id;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
/// [Bep_0044](https://www.bittorrent.org/beps/bep_0044.html)'s Mutable item.
pub struct MutableItem {
    /// hash of the key and optional salt
    target: Id,
    /// ed25519 public key
    key: [u8; 32],
    /// sequence number
    seq: i64,
    /// mutable value
    value: Box<[u8]>,
    /// ed25519 signature
    #[serde(with = "serde_bytes")]
    signature: [u8; 64],
    /// Optional salt
    salt: Option<Box<[u8]>>,
    /// Optional compare and swap seq
    cas: Option<i64>,
}

impl MutableItem {
    /// Create a new mutable item from a signing key, value, sequence number and optional salt.
    pub fn new(signer: SigningKey, value: &[u8], seq: i64, salt: Option<&[u8]>) -> Self {
        let signable = encode_signable(seq, value, salt);
        let signature = signer.sign(&signable);

        Self::new_signed_unchecked(
            signer.verifying_key().to_bytes(),
            signature.into(),
            value,
            seq,
            salt,
        )
    }

    /// Return the target of a [MutableItem] by hashing its `public_key` and an optional `salt`
    pub fn target_from_key(public_key: &[u8; 32], salt: Option<&[u8]>) -> Id {
        let mut encoded = vec![];

        encoded.extend(public_key);

        if let Some(salt) = salt {
            encoded.extend(salt);
        }

        let mut hasher = Sha1::new();
        hasher.update(&encoded);
        let bytes = hasher.digest().bytes();

        bytes.into()
    }

    /// Set the cas number if needed.
    pub fn with_cas(mut self, cas: i64) -> Self {
        self.cas = Some(cas);
        self
    }

    /// Create a new mutable item from an already signed value.
    pub fn new_signed_unchecked(
        key: [u8; 32],
        signature: [u8; 64],
        value: &[u8],
        seq: i64,
        salt: Option<&[u8]>,
    ) -> Self {
        Self {
            target: MutableItem::target_from_key(&key, salt),
            key,
            value: value.to_vec().into_boxed_slice(),
            seq,
            signature,
            salt: salt.map(|s| s.to_vec().into_boxed_slice()),
            cas: None,
        }
    }

    pub(crate) fn from_dht_message(
        target: Id,
        key: &[u8],
        v: Box<[u8]>,
        seq: i64,
        signature: &[u8],
        salt: Option<&[u8]>,
        cas: Option<i64>,
    ) -> Result<Self, MutableError> {
        let key = VerifyingKey::try_from(key).map_err(|_| MutableError::InvalidMutablePublicKey)?;

        let signature =
            Signature::from_slice(signature).map_err(|_| MutableError::InvalidMutableSignature)?;

        key.verify(&encode_signable(seq, &v, salt), &signature)
            .map_err(|_| MutableError::InvalidMutableSignature)?;

        Ok(Self {
            target,
            key: key.to_bytes(),
            value: v,
            seq,
            signature: signature.to_bytes(),
            salt: salt.map(|s| s.to_vec().into_boxed_slice()),
            cas,
        })
    }

    // === Getters ===

    pub fn target(&self) -> &Id {
        &self.target
    }

    pub fn key(&self) -> &[u8; 32] {
        &self.key
    }

    pub fn value(&self) -> &[u8] {
        &self.value
    }

    pub fn seq(&self) -> i64 {
        self.seq
    }

    pub fn signature(&self) -> &[u8; 64] {
        &self.signature
    }

    pub fn salt(&self) -> Option<&[u8]> {
        self.salt.as_deref()
    }

    pub fn cas(&self) -> Option<i64> {
        self.cas
    }
}

pub fn encode_signable(seq: i64, value: &[u8], salt: Option<&[u8]>) -> Box<[u8]> {
    let mut signable = vec![];

    if let Some(salt) = salt {
        signable.extend(format!("4:salt{}:", salt.len()).into_bytes());
        signable.extend(salt);
    }

    signable.extend(format!("3:seqi{}e1:v{}:", seq, value.len()).into_bytes());
    signable.extend(value);

    signable.into()
}

#[derive(thiserror::Error, Debug)]
/// Mainline crate error enum.
pub enum MutableError {
    #[error("Invalid mutable item signature")]
    InvalidMutableSignature,

    #[error("Invalid mutable item public key")]
    InvalidMutablePublicKey,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signable_without_salt() {
        let signable = encode_signable(4, b"Hello world!", None);

        assert_eq!(&*signable, b"3:seqi4e1:v12:Hello world!");
    }
    #[test]
    fn signable_with_salt() {
        let signable = encode_signable(4, b"Hello world!", Some(b"foobar"));

        assert_eq!(&*signable, b"4:salt6:foobar3:seqi4e1:v12:Hello world!");
    }
}
