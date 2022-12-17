use anyhow::Result;
use crypto::hash::Hash;
use crypto::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use std::fmt::{self, Debug, Display};
use std::marker::PhantomData;

#[derive(Serialize, Deserialize)]
/// A Signature contains (source, raw signature bytes)
pub struct Signature<Id, T> {
    pub(super) raw: Vec<u8>,
    pub(super) id: Id,
    pub(super) _x: PhantomData<T>,
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
        let raw = sk.sign(msg_hash.as_ref())?;
        Ok(Self {
            raw,
            id,
            _x: PhantomData,
        })
    }

    pub fn get_id(&self) -> Id
    where
        Id: Clone,
    {
        self.id.clone()
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
        if !pk.verify(msg_hash.as_ref(), &self.raw) {
            return Err(anyhow::Error::msg("Signature Verification Failed"));
        }
        Ok(())
    }
}

impl<Id, T> Debug for Signature<Id, T>
where
    Id: Debug,
{
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        f.debug_struct("Signature")
            .field("sig", &base64::encode(&self.raw))
            .field("id", &self.id)
            .finish()
    }
}

impl<Id, T> Display for Signature<Id, T>
where
    Id: Debug,
{
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        f.debug_struct("Signature")
            .field("sig", &base64::encode(&self.raw))
            .field("id", &self.id)
            .finish()
    }
}
