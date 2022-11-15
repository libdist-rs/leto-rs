use crypto::{ed25519, secp256k1, Algorithm, Keypair, PublicKey, SecretKey};
use fnv::FnvHashMap;
use serde::{Deserialize, Serialize};

use crate::Id;

#[derive(Debug, Serialize, Deserialize, Clone)]
struct RawKeyConfig {
    pub alg: Algorithm,
    pub secret_bytes: Vec<u8>,
    pub system: FnvHashMap<Id, Vec<u8>>,
}

#[derive(Clone)]
pub struct KeyConfig {
    pub alg: Algorithm,
    pub secret: SecretKey,
    pub system: FnvHashMap<Id, PublicKey>,
}

impl KeyConfig {
    pub fn generate(
        alg: Algorithm,
        num_nodes: usize,
    ) -> anyhow::Result<Vec<Self>> {
        let mut sks = FnvHashMap::default();
        let mut system = FnvHashMap::default();
        let mut configs = Vec::with_capacity(num_nodes);
        for i in 0..num_nodes {
            let id: Id = i.into();
            let kpair = match alg {
                Algorithm::RSA => {
                    #[cfg(feature = "RSA")]
                    todo!();
                    #[cfg(not(feature = "RSA"))]
                    unreachable!()
                }
                Algorithm::ED25519 => Keypair::generate_ed25519()?,
                Algorithm::SECP256K1 => Keypair::generate_secp256k1(),
            };
            let (sk, pk) = (kpair.private(), kpair.public());
            sks.insert(id, sk);
            system.insert(id, pk);
        }
        for i in 0..num_nodes {
            let id: Id = i.into();
            configs.push(Self {
                alg: alg.clone(),
                secret: sks.remove(&id).unwrap(),
                system: system.clone(),
            });
        }
        Ok(configs)
    }
}

impl Serialize for KeyConfig {
    fn serialize<S>(
        &self,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut new_system = FnvHashMap::default();
        for (k, v) in &self.system {
            let pub_key_bytes = match v {
                PublicKey::Ed25519(pk) => bincode::serialize(&pk).unwrap(),
                PublicKey::Secp256k1(pk) => pk.encode().to_vec(),
                #[cfg(feature = "RSA")]
                PublicKey::RSA(pk) => {
                    todo!();
                }
            };
            new_system.insert(k.clone(), pub_key_bytes);
        }
        let sk_bytes = match &self.secret {
            SecretKey::Ed25519(sk) => bincode::serialize(sk).unwrap(),
            SecretKey::Secp256k1(sk) => sk.to_bytes().to_vec(),
            #[cfg(feature = "RSA")]
            SecretKey::RSA(sk) => {}
        };
        let raw = RawKeyConfig {
            alg: self.alg.clone(),
            secret_bytes: sk_bytes,
            system: new_system,
        };
        raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for KeyConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut raw = RawKeyConfig::deserialize(deserializer)?;
        let sk = match raw.alg {
            Algorithm::ED25519 => {
                let skey: crypto::ed25519::SecretKey =
                    bincode::deserialize(&raw.secret_bytes).unwrap();
                crypto::SecretKey::Ed25519(skey)
            }
            Algorithm::SECP256K1 => {
                let skey = crypto::secp256k1::SecretKey::from_bytes(&mut raw.secret_bytes).unwrap();
                crypto::SecretKey::Secp256k1(skey)
            }
            #[cfg(feature = "RSA")]
            Algorithm::RSA => {}
            #[cfg(not(feature = "RSA"))]
            _ => unreachable!(),
        };
        let mut new_system = FnvHashMap::default();
        for (id, pk_bytes) in raw.system {
            let pk = match raw.alg {
                Algorithm::ED25519 => {
                    let pk: ed25519::PublicKey = bincode::deserialize(&pk_bytes).unwrap();
                    PublicKey::Ed25519(pk)
                }
                Algorithm::SECP256K1 => {
                    let pk = secp256k1::PublicKey::decode(&pk_bytes).unwrap();
                    PublicKey::Secp256k1(pk)
                }
                Algorithm::RSA => {
                    #[cfg(feature = "RSA")]
                    ();
                    #[cfg(not(feature = "RSA"))]
                    unreachable!();
                }
            };
            new_system.insert(id, pk);
        }
        Ok(Self {
            alg: raw.alg,
            secret: sk,
            system: new_system,
        })
    }
}

#[test]
fn test_codec() -> anyhow::Result<()> {
    use crypto::Keypair;

    let num_nodes: usize = 4;

    let alg1 = Algorithm::ED25519;
    let alg2 = Algorithm::SECP256K1;

    let mut ed_sks = FnvHashMap::default();
    let mut ed_system = FnvHashMap::default();

    let mut secp_sks = FnvHashMap::default();
    let mut secp_system = FnvHashMap::default();

    for i in 0..num_nodes {
        let id: Id = i.into();

        // Ed
        let ed_kpair = Keypair::generate_ed25519()?;
        let (ed_sk, ed_pk) = (ed_kpair.private(), ed_kpair.public());
        ed_system.insert(id, ed_pk);
        ed_sks.insert(id, ed_sk);

        // Secp
        let secp_kpair = Keypair::generate_secp256k1();
        let (secp_sk, secp_pk) = (secp_kpair.private(), secp_kpair.public());
        secp_system.insert(id, secp_pk);
        secp_sks.insert(id, secp_sk);
    }

    for i in 0..num_nodes {
        let id: Id = i.into();
        let ed_config = KeyConfig {
            alg: alg1.clone(),
            secret: ed_sks[&id].clone(),
            system: ed_system.clone(),
        };

        let bytes = bincode::serialize(&ed_config)?;
        let ed_config2: KeyConfig = bincode::deserialize(&bytes)?;
        assert_eq!(ed_config.alg, ed_config2.alg);
        assert_eq!(ed_config.system, ed_config2.system);
        let bytes2 = bincode::serialize(&ed_config2)?;
        assert_eq!(bytes, bytes2);

        let secp_config = KeyConfig {
            alg: alg2.clone(),
            secret: secp_sks[&id].clone(),
            system: secp_system.clone(),
        };

        let bytes = bincode::serialize(&secp_config)?;
        let secp_config2: KeyConfig = bincode::deserialize(&bytes)?;
        assert_eq!(secp_config.alg, secp_config2.alg);
        assert_eq!(secp_config.system, secp_config2.system);
        let bytes2 = bincode::serialize(&secp_config2)?;
        assert_eq!(bytes, bytes2);
    }
    Ok(())
}
