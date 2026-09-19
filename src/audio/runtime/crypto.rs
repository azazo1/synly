use super::AudioChannelDirection;
use anyhow::{Result, anyhow};
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use ring::hkdf;

const AUDIO_AAD: &[u8] = b"synly-audio-udp-v2";
const AUDIO_COUNTER_LEN: usize = 8;
const REPLAY_WINDOW_BITS: u64 = 64;

// channel_id 由接收端每次绑定时生成, 仅通过 TLS 控制通道交付发送端.
// 整个标识参与密钥派生, 不将代次压缩为短 nonce 前缀.
pub(super) fn derive_channel_secret(master_secret: [u8; 32], channel_id: [u8; 32]) -> Result<[u8; 32]> {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, b"synly-audio-channel-v2");
    hkdf_expand(&salt.extract(&master_secret), &[&channel_id])
}

pub(super) struct AudioEncryptor {
    key: LessSafeKey,
    nonce_prefix: [u8; 4],
    counter: u64,
}

impl AudioEncryptor {
    // 每个已协商通道和方向只能创建一个发送器. 重建须先通过 TLS 协商新的 channel_id.
    pub fn new(channel_secret: [u8; 32], direction: AudioChannelDirection) -> Result<Self> {
        let (key, nonce_prefix) = derive_directional_key(channel_secret, direction)?;
        Ok(Self { key, nonce_prefix, counter: 0 })
    }

    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let counter = self.counter;
        self.counter = self.counter.checked_add(1)
            .ok_or_else(|| anyhow!("音频 UDP nonce 计数器已耗尽"))?;
        let mut in_out = plaintext.to_vec();
        self.key.seal_in_place_append_tag(
            build_nonce(self.nonce_prefix, counter), Aad::from(AUDIO_AAD), &mut in_out,
        ).map_err(|_| anyhow!("音频 UDP 加密失败"))?;
        let mut packet = Vec::with_capacity(AUDIO_COUNTER_LEN + in_out.len());
        packet.extend_from_slice(&counter.to_be_bytes());
        packet.extend_from_slice(&in_out);
        Ok(packet)
    }
}

pub(super) struct AudioDecryptor {
    key: LessSafeKey,
    nonce_prefix: [u8; 4],
    highest_counter: Option<u64>,
    seen_window: u64,
}

impl AudioDecryptor {
    pub fn new(channel_secret: [u8; 32], direction: AudioChannelDirection) -> Result<Self> {
        let (key, nonce_prefix) = derive_directional_key(channel_secret, direction)?;
        Ok(Self { key, nonce_prefix, highest_counter: None, seen_window: 0 })
    }

    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        if ciphertext.len() < AUDIO_COUNTER_LEN + CHACHA20_POLY1305.tag_len() {
            return Err(anyhow!("音频 UDP 密文长度不足"));
        }
        let (counter_bytes, body) = ciphertext.split_at(AUDIO_COUNTER_LEN);
        let counter = u64::from_be_bytes(counter_bytes.try_into().unwrap());
        if let Some(highest) = self.highest_counter
            && counter <= highest
        {
            let distance = highest - counter;
            if distance >= REPLAY_WINDOW_BITS || self.seen_window & (1 << distance) != 0 {
                return Err(anyhow!("拒绝重放或超出乱序窗口的音频 UDP 包"));
            }
        }
        let mut in_out = body.to_vec();
        let plaintext = self.key.open_in_place(
            build_nonce(self.nonce_prefix, counter), Aad::from(AUDIO_AAD), &mut in_out,
        ).map_err(|_| anyhow!("音频 UDP 认证失败"))?;
        // 只有认证通过的包才能推进窗口, 伪造大计数器不能淘汰正常音频.
        self.record_authenticated_packet(counter);
        Ok(plaintext.to_vec())
    }

    fn record_authenticated_packet(&mut self, counter: u64) {
        match self.highest_counter {
            None => {
                self.highest_counter = Some(counter);
                self.seen_window = 1;
            }
            Some(highest) if counter > highest => {
                let shift = counter - highest;
                self.seen_window = if shift >= REPLAY_WINDOW_BITS { 1 } else { (self.seen_window << shift) | 1 };
                self.highest_counter = Some(counter);
            }
            Some(highest) => self.seen_window |= 1 << (highest - counter),
        }
    }
}

fn derive_directional_key(channel_secret: [u8; 32], direction: AudioChannelDirection) -> Result<(LessSafeKey, [u8; 4])> {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, b"synly-audio-udp-key-v2");
    let prk = salt.extract(&channel_secret);
    let key_bytes = hkdf_expand::<32>(&prk, &[b"key", direction.as_label()])?;
    let nonce_prefix = hkdf_expand::<4>(&prk, &[b"nonce", direction.as_label()])?;
    let key = LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &key_bytes)
        .map_err(|_| anyhow!("无法初始化音频 AEAD 密钥"))?);
    Ok((key, nonce_prefix))
}

fn build_nonce(prefix: [u8; 4], counter: u64) -> Nonce {
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&prefix);
    nonce[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::assume_unique_for_key(nonce)
}

fn hkdf_expand<const N: usize>(prk: &hkdf::Prk, info: &[&[u8]]) -> Result<[u8; N]> {
    let mut output = [0u8; N];
    prk.expand(info, HkdfLen(N)).map_err(|_| anyhow!("音频 HKDF 扩展失败"))?
        .fill(&mut output).map_err(|_| anyhow!("音频 HKDF 输出失败"))?;
    Ok(output)
}

#[derive(Clone, Copy)]
struct HkdfLen(usize);
impl hkdf::KeyType for HkdfLen {
    fn len(&self) -> usize { self.0 }
}

#[cfg(test)]
mod tests;
