use super::{DesktopLayout, InputChannelRole, ModifierMask, ScreenEdge};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_INPUT_FRAME_LEN: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeySnapshot {
    pub usages: Vec<u16>,
    pub modifiers: ModifierMask,
    pub buttons: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum InputMessage {
    Proof {
        role: InputChannelRole,
        proof: [u8; 32],
    },
    Layout(DesktopLayout),
    Activate {
        generation: u64,
        source_edge: ScreenEdge,
        edge_position: f32,
        pressed: KeySnapshot,
    },
    Deactivate {
        generation: u64,
    },
    Return {
        generation: u64,
        edge_position: f32,
    },
    Heartbeat {
        generation: u64,
    },
    Key {
        generation: u64,
        usage: u16,
        modifiers: ModifierMask,
        down: bool,
        repeat: bool,
    },
    Button {
        generation: u64,
        button: u8,
        down: bool,
    },
    Motion {
        generation: u64,
        dx: i32,
        dy: i32,
    },
    Wheel {
        generation: u64,
        x: i32,
        y: i32,
    },
}

pub async fn write_message<W>(writer: &mut W, message: &InputMessage) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let bytes = bincode::serialize(message).context("无法编码输入消息")?;
    if bytes.len() > MAX_INPUT_FRAME_LEN {
        bail!("输入消息超过 64 KiB 限制");
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_message<R>(reader: &mut R) -> Result<InputMessage>
where
    R: AsyncRead + Unpin,
{
    let len = reader.read_u32().await? as usize;
    if len == 0 || len > MAX_INPUT_FRAME_LEN {
        bail!("输入消息长度无效: {len}");
    }
    let mut bytes = vec![0u8; len];
    reader.read_exact(&mut bytes).await?;
    bincode::deserialize(&bytes).context("无法解码输入消息")
}

#[cfg(test)]
mod tests {
    use super::{InputMessage, MAX_INPUT_FRAME_LEN, read_message, write_message};
    use tokio::io::{AsyncWriteExt, duplex};

    #[tokio::test]
    async fn input_messages_roundtrip() {
        let (mut left, mut right) = duplex(4096);
        let message = InputMessage::Motion { generation: 7, dx: -12, dy: 34 };
        let expected = message.clone();
        let write = tokio::spawn(async move { write_message(&mut left, &message).await });
        let actual = read_message(&mut right).await.unwrap();
        write.await.unwrap().unwrap();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_before_allocation() {
        let (mut writer, mut reader) = duplex(16);
        writer.write_u32((MAX_INPUT_FRAME_LEN + 1) as u32).await.unwrap();
        assert!(read_message(&mut reader).await.is_err());
    }
}
