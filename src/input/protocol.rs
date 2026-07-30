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
    let message_name = message_name(message);
    let bytes = bincode::serialize(message).context("无法编码输入消息")?;
    if bytes.len() > MAX_INPUT_FRAME_LEN {
        tracing::trace!(message = message_name, length = bytes.len(), "输入辅助 frame 长度超过限制"); // to remove
        bail!("输入消息超过 64 KiB 限制");
    }
    writer
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await
        .map_err(|error| {
            tracing::trace!(message = message_name, error = %error, "输入辅助 frame 长度写入失败"); // to remove
            error
        })?;
    writer.write_all(&bytes).await.map_err(|error| {
        tracing::trace!(message = message_name, error = %error, "输入辅助 frame body 写入失败"); // to remove
        error
    })?;
    writer.flush().await.map_err(|error| {
        tracing::trace!(message = message_name, error = %error, "输入辅助 frame flush 失败"); // to remove
        error
    })?;
    Ok(())
}

pub async fn read_message<R>(reader: &mut R) -> Result<InputMessage>
where
    R: AsyncRead + Unpin,
{
    let mut length_prefix = [0u8; 4];
    reader.read_exact(&mut length_prefix).await.map_err(|error| {
        tracing::trace!(error = %error, "输入辅助 frame 长度读取失败"); // to remove
        error
    })?;
    let len = u32::from_be_bytes(length_prefix) as usize;
    if len == 0 || len > MAX_INPUT_FRAME_LEN {
        tracing::trace!(len, length_prefix = ?length_prefix, "输入辅助 frame 长度校验失败"); // to remove
        bail!("输入消息长度无效: {len}, 原始长度前缀: {length_prefix:?}");
    }
    let mut bytes = vec![0u8; len];
    reader.read_exact(&mut bytes).await.map_err(|error| {
        tracing::trace!(len, error = %error, "输入辅助 frame body 读取失败"); // to remove
        error
    })?;
    bincode::deserialize(&bytes)
        .inspect_err(|error| {
            tracing::trace!(len, error = %error, "输入辅助 frame 解码失败"); // to remove
        })
        .context("无法解码输入消息")
}

fn message_name(message: &InputMessage) -> &'static str {
    match message {
        InputMessage::Proof { .. } => "Proof",
        InputMessage::Layout(_) => "Layout",
        InputMessage::Activate { .. } => "Activate",
        InputMessage::Deactivate { .. } => "Deactivate",
        InputMessage::Return { .. } => "Return",
        InputMessage::Heartbeat { .. } => "Heartbeat",
        InputMessage::Key { .. } => "Key",
        InputMessage::Button { .. } => "Button",
        InputMessage::Motion { .. } => "Motion",
        InputMessage::Wheel { .. } => "Wheel",
    }
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
        writer
            .write_all(&((MAX_INPUT_FRAME_LEN + 1) as u32).to_be_bytes())
            .await
            .unwrap();
        assert!(read_message(&mut reader).await.is_err());
    }
}
