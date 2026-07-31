use crate::device::DeviceConfig;
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use uuid::Uuid;

pub fn generate_identity_keypair() -> Result<(String, String)> {
    let rng = SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| anyhow::anyhow!("failed to generate device identity key"))?;
    let private_key = STANDARD_NO_PAD.encode(pkcs8.as_ref());
    let public_key = public_key_from_private(&private_key)?;
    Ok((private_key, public_key))
}

pub fn validate_keypair(private_key: &str, public_key: &str) -> Result<()> {
    if private_key.trim().is_empty() {
        bail!("device identity private key is empty");
    }
    if public_key.trim().is_empty() {
        bail!("device identity public key is empty");
    }
    let derived = public_key_from_private(private_key)?;
    if derived != public_key.trim() {
        bail!("device identity public key does not match the private key");
    }
    Ok(())
}

pub fn public_key_from_private(private_key: &str) -> Result<String> {
    let pkcs8 = STANDARD_NO_PAD
        .decode(private_key.trim().as_bytes())
        .context("failed to decode device identity private key")?;
    let key_pair = Ed25519KeyPair::from_pkcs8(&pkcs8)
        .map_err(|_| anyhow::anyhow!("failed to parse device identity private key"))?;
    Ok(STANDARD_NO_PAD.encode(key_pair.public_key().as_ref()))
}

pub fn generate_device_config(device_name: String) -> Result<DeviceConfig> {
    let (identity_private_key, identity_public_key) = generate_identity_keypair()?;
    Ok(DeviceConfig {
        device_id: Uuid::new_v4(),
        device_name,
        identity_private_key,
        identity_public_key,
    })
}

#[cfg(test)]
mod tests {
    use super::{generate_device_config, validate_keypair};

    #[test]
    fn generated_device_identity_is_self_consistent() {
        let device = generate_device_config("测试设备".to_string()).expect("generate");
        validate_keypair(&device.identity_private_key, &device.identity_public_key)
            .expect("keypair is self consistent");
    }
}
