//! Cryptographically secure Instance identities.
use yokai_types::InstanceId;

pub fn random_instance_id() -> Result<InstanceId, getrandom::Error> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)?;
    Ok(InstanceId::from_bytes(bytes))
}
