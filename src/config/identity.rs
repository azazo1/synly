use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};

pub(super) fn generate_keypair() -> Result<(String, String)> {
    let rng = SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| anyhow!("failed to generate device identity key"))?;
    let private_key = STANDARD_NO_PAD.encode(pkcs8.as_ref());
    let public_key = public_key_from_private(&private_key)?;
    Ok((private_key, public_key))
}

pub(super) fn validate_keypair(private_key: &str, public_key: &str) -> Result<()> {
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

fn public_key_from_private(private_key: &str) -> Result<String> {
    let pkcs8 = STANDARD_NO_PAD
        .decode(private_key.trim().as_bytes())
        .context("failed to decode device identity private key")?;
    let key_pair = Ed25519KeyPair::from_pkcs8(&pkcs8)
        .map_err(|_| anyhow!("failed to parse device identity private key"))?;
    Ok(STANDARD_NO_PAD.encode(key_pair.public_key().as_ref()))
}
