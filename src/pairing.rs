use std::fmt;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::trust::DeviceId;

const SAS_DOMAIN: &[u8] = b"rflow pairing sas v1\0";
pub const PAIRING_PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairingRole {
    Server,
    Client,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingMaterial {
    pub role: PairingRole,
    pub certificate: Vec<u8>,
    pub nonce: [u8; 32],
}

impl PairingMaterial {
    pub fn generate(role: PairingRole, certificate: Vec<u8>) -> Result<Self> {
        let mut nonce = [0_u8; 32];
        getrandom::fill(&mut nonce)
            .map_err(|error| anyhow::anyhow!("generate pairing nonce: {error}"))?;
        Ok(Self {
            role,
            certificate,
            nonce,
        })
    }

    pub fn hello(&self, device_name: String) -> PairingMessage {
        PairingMessage::Hello {
            version: PAIRING_PROTOCOL_VERSION,
            role: self.role,
            device_name,
            nonce: self.nonce,
            certificate_fingerprint: DeviceId::from_certificate(&self.certificate),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairingMessage {
    Hello {
        version: u16,
        role: PairingRole,
        device_name: String,
        nonce: [u8; 32],
        certificate_fingerprint: DeviceId,
    },
    Accepted,
    Acknowledged,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PairingRequestId(pub u64);

impl fmt::Display for PairingRequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "p-{:016x}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingProof {
    pub request_id: PairingRequestId,
    pub code: PairingCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingCode(u32);

impl fmt::Display for PairingCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:03} {:03}", self.0 / 1_000, self.0 % 1_000)
    }
}

pub fn pairing_code(first: &PairingMaterial, second: &PairingMaterial) -> Result<PairingCode> {
    Ok(pairing_proof(first, second)?.code)
}

pub fn pairing_proof(first: &PairingMaterial, second: &PairingMaterial) -> Result<PairingProof> {
    let (server, client) = match (first.role, second.role) {
        (PairingRole::Server, PairingRole::Client) => (first, second),
        (PairingRole::Client, PairingRole::Server) => (second, first),
        _ => bail!("pairing transcript requires one server and one client"),
    };
    let mut transcript = Sha256::new();
    transcript.update(SAS_DOMAIN);
    update_length_prefixed(&mut transcript, &server.certificate)?;
    update_length_prefixed(&mut transcript, &client.certificate)?;
    transcript.update(server.nonce);
    transcript.update(client.nonce);
    let digest = transcript.finalize();
    let prefix = u32::from_be_bytes(
        digest[..4]
            .try_into()
            .expect("SHA-256 prefix is four bytes"),
    );
    let request_id = PairingRequestId(u64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("SHA-256 request prefix is eight bytes"),
    ));
    Ok(PairingProof {
        request_id,
        code: PairingCode(prefix % 1_000_000),
    })
}

fn update_length_prefixed(hasher: &mut Sha256, bytes: &[u8]) -> Result<()> {
    let length =
        u32::try_from(bytes.len()).map_err(|_| anyhow::anyhow!("pairing field too large"))?;
    hasher.update(length.to_be_bytes());
    hasher.update(bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_roles_derive_the_same_transcript_bound_code() {
        let server = PairingMaterial {
            role: PairingRole::Server,
            certificate: b"server certificate".to_vec(),
            nonce: [0x11; 32],
        };
        let client = PairingMaterial {
            role: PairingRole::Client,
            certificate: b"client certificate".to_vec(),
            nonce: [0x22; 32],
        };
        let code = pairing_code(&server, &client).unwrap();
        assert_eq!(code, pairing_code(&client, &server).unwrap());
        assert_eq!(code.to_string().len(), 7);

        let mut changed_client = client.clone();
        changed_client.nonce[0] ^= 1;
        assert_ne!(code, pairing_code(&server, &changed_client).unwrap());
    }
}
