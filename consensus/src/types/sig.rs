use super::Hash;
use anyhow::Result;
use crypto::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use std::marker::PhantomData;

#[derive(Debug, Serialize, Deserialize)]
/// A Signature contains (source, raw signature bytes)
pub struct Signature<Id, T> {
    raw: Vec<u8>,
    pub(crate) id: Id,
    _x: PhantomData<T>,
}

impl<Id, T> Clone for Signature<Id, T>
where
    Id: Clone,
{
    fn clone(&self) -> Self {
        Self {
            raw: self.raw.clone(),
            id: self.id.clone(),
            _x: PhantomData,
        }
    }
}

impl<Id, T> Signature<Id, T> {
    pub fn new(
        msg_hash: Hash<T>,
        id: Id,
        sk: &SecretKey,
    ) -> Result<Self> {
        let raw = sk.sign(&msg_hash.hash)?;
        Ok(Self {
            raw,
            id,
            _x: PhantomData,
        })
    }
}

impl<Id, T> Signature<Id, T>
where
    Id: std::cmp::PartialEq,
{
    /// Verify ensures that this is indeed the signature for the given message
    /// with hash `msg_hash` signed by `pk`
    pub fn verify(
        &self,
        msg_hash: &Hash<T>,
        id: &Id,
        pk: &PublicKey,
    ) -> Result<()> {
        if self.id != *id {
            return Err(anyhow::Error::msg("The Id(s) do(es) not match"));
        }
        self.verify_without_id_check(msg_hash, pk)
    }

    /// This function checks that the signature is for the given message
    /// `msg_hash` signed by `pk` Potential applications of this function:
    /// Threshold signatures
    pub fn verify_without_id_check(
        &self,
        msg_hash: &Hash<T>,
        pk: &PublicKey,
    ) -> Result<()> {
        if !pk.verify(&msg_hash.hash, &self.raw) {
            return Err(anyhow::Error::msg("Signature Verification Failed"));
        }
        Ok(())
    }
}
