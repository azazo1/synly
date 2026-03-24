use crate::cli::{AudioMode, SyncMode};
use crate::sync::{ManifestSnapshot, WorkspaceSummary};
use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

const FRAME_CONTROL: u8 = 1;
const FRAME_FILE_CHUNK: u8 = 2;
const FRAME_CLIPBOARD_META: u8 = 3;
const FRAME_CLIPBOARD_CHUNK: u8 = 4;
const DEFAULT_MAX_META_LEN: usize = 20 * 1024 * 1024;
const DEFAULT_MAX_FRAME_DATA_LEN: usize = 128 * 1024 * 1024;
const DEFAULT_MAX_CLIPBOARD_BINARY_LEN: usize = 100 * 1024 * 1024;
const CLIPBOARD_STREAM_CHUNK_SIZE: usize = 1024 * 1024;

pub const PROTOCOL_VERSION: u16 = 14;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferLimits {
    pub max_meta_len: usize,
    pub max_frame_data_len: usize,
    pub max_clipboard_binary_len: usize,
}

impl Default for TransferLimits {
    fn default() -> Self {
        Self {
            max_meta_len: DEFAULT_MAX_META_LEN,
            max_frame_data_len: DEFAULT_MAX_FRAME_DATA_LEN,
            max_clipboard_binary_len: DEFAULT_MAX_CLIPBOARD_BINARY_LEN,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PairAuthMethod {
    #[default]
    Pin,
    TrustedDevice,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceIdentity {
    pub device_id: Uuid,
    pub device_name: String,
    pub instance_name: Option<String>,
    pub identity_public_key: String,
    pub tls_root_certificate: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairRequestPayload {
    pub protocol_version: u16,
    pub client: DeviceIdentity,
    pub requested_mode: SyncMode,
    pub workspace: WorkspaceSummary,
    #[serde(default)]
    pub audio_mode: AudioMode,
    #[serde(default)]
    pub request_trust: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionAgreement {
    pub host_to_client: bool,
    pub client_to_host: bool,
}

impl SessionAgreement {
    pub fn any_direction(&self) -> bool {
        self.host_to_client || self.client_to_host
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlMessage {
    BootstrapHello {
        protocol_version: u16,
        client_bootstrap_public_key: String,
    },
    BootstrapChallenge {
        request_id: String,
        server_bootstrap_public_key: String,
        server_pake_message: String,
    },
    BootstrapPake {
        request_id: String,
        client_pake_message: String,
        client_confirm: String,
    },
    BootstrapAck {
        request_id: String,
        server_confirm: String,
    },
    PairRequest {
        request_id: String,
        payload: PairRequestPayload,
        trusted_proof: Option<String>,
    },
    PinChallenge {
        request_id: String,
        server: DeviceIdentity,
        message: String,
    },
    PairAuth {
        request_id: String,
        proof: String,
    },
    PairDecision {
        accepted: bool,
        message: String,
        server: DeviceIdentity,
        workspace: WorkspaceSummary,
        agreement: SessionAgreement,
        #[serde(default)]
        audio_mode: AudioMode,
        #[serde(default)]
        auth_method: PairAuthMethod,
        #[serde(default)]
        server_trusts_client: bool,
        proof: String,
        #[serde(default)]
        trust_established: bool,
    },
    AudioUdpReady {
        port: u16,
    },
    SnapshotAdvert {
        revision: u64,
        snapshot: ManifestSnapshot,
        sender_time_ms: u64,
    },
    FileRequest {
        revision: u64,
        paths: Vec<String>,
    },
    OverwritePaused {
        revision: u64,
        paths: Vec<String>,
    },
    TransferDone {
        revision: u64,
    },
    TransferAborted {
        revision: u64,
        message: String,
    },
    Error {
        message: String,
    },
    Goodbye,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileChunkHeader {
    pub revision: u64,
    pub path: String,
    pub offset: u64,
    pub total_size: u64,
    pub modified_ms: u64,
    pub executable: bool,
    pub final_chunk: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardPayload {
    pub text: Option<String>,
    pub rich_text: Option<String>,
    pub html: Option<String>,
    pub image: Option<ClipboardImage>,
    pub files: Vec<ClipboardFile>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardImage {
    pub png_bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum Frame {
    Control(ControlMessage),
    FileChunk(FileChunkHeader, Vec<u8>),
    Clipboard(ClipboardPayload),
}

pub struct FrameReader<R> {
    inner: R,
    limits: TransferLimits,
}

pub struct FrameWriter<W> {
    inner: W,
    limits: TransferLimits,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ClipboardPayloadMeta {
    pub text: Option<String>,
    pub rich_text: Option<String>,
    pub html: Option<String>,
    pub image: Option<ClipboardBinaryMeta>,
    pub files: Vec<ClipboardFileMeta>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ClipboardBinaryMeta {
    pub offset: u64,
    pub len: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ClipboardFileMeta {
    pub name: String,
    pub data: ClipboardBinaryMeta,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ClipboardTransferHeader {
    pub transfer_id: Uuid,
    pub payload: ClipboardPayloadMeta,
    pub binary_len: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ClipboardChunkHeader {
    pub transfer_id: Uuid,
    pub offset: u64,
    pub final_chunk: bool,
}

#[derive(Debug)]
pub struct FrameSizeLimitError {
    context: &'static str,
    actual: usize,
    limit: usize,
}

impl FrameSizeLimitError {
    fn new(context: &'static str, actual: usize, limit: usize) -> Self {
        Self {
            context,
            actual,
            limit,
        }
    }
}

impl fmt::Display for FrameSizeLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} exceeds limit: {} > {}",
            self.context, self.actual, self.limit
        )
    }
}

impl std::error::Error for FrameSizeLimitError {}

impl ClipboardPayload {
    pub fn is_empty(&self) -> bool {
        self.text.is_none()
            && self.rich_text.is_none()
            && self.html.is_none()
            && self.image.is_none()
            && self.files.is_empty()
    }

    pub fn total_binary_size(&self) -> usize {
        let image_len = self
            .image
            .as_ref()
            .map(|image| image.png_bytes.len())
            .unwrap_or_default();
        image_len
            + self
                .files
                .iter()
                .map(|file| file.bytes.len())
                .sum::<usize>()
    }

    fn into_wire(self) -> Result<(ClipboardPayloadMeta, Vec<u8>)> {
        let mut data = Vec::with_capacity(self.total_binary_size());
        let image = self
            .image
            .map(|image| append_binary(&mut data, image.png_bytes))
            .transpose()?;
        let files = self
            .files
            .into_iter()
            .map(|file| {
                Ok(ClipboardFileMeta {
                    name: file.name,
                    data: append_binary(&mut data, file.bytes)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let meta = ClipboardPayloadMeta {
            text: self.text,
            rich_text: self.rich_text,
            html: self.html,
            image,
            files,
        };
        Ok((meta, data))
    }

    fn from_wire(meta: ClipboardPayloadMeta, data: Vec<u8>) -> Result<Self> {
        let image = meta
            .image
            .map(|binary| {
                read_binary(&data, binary).map(|bytes| ClipboardImage {
                    png_bytes: bytes.to_vec(),
                })
            })
            .transpose()?;
        let files = meta
            .files
            .into_iter()
            .map(|file| {
                Ok(ClipboardFile {
                    name: file.name,
                    bytes: read_binary(&data, file.data)?.to_vec(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            text: meta.text,
            rich_text: meta.rich_text,
            html: meta.html,
            image,
            files,
        })
    }
}

pub fn encode_payload<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    bincode::serialize(value).context("failed to serialize payload")
}

pub fn decode_payload<T: DeserializeOwned>(bytes: &[u8], context: &'static str) -> Result<T> {
    bincode::deserialize(bytes).with_context(|| context.to_string())
}

pub fn frame_size_limit_message(err: &anyhow::Error) -> Option<String> {
    err.downcast_ref::<FrameSizeLimitError>()
        .map(ToString::to_string)
}

#[derive(Debug)]
struct RawFrame {
    frame_type: u8,
    meta: Vec<u8>,
    data: Vec<u8>,
}

impl<R> FrameReader<R> {
    #[allow(dead_code)]
    pub fn new(inner: R) -> Self {
        Self::with_limits(inner, TransferLimits::default())
    }

    pub fn with_limits(inner: R, limits: TransferLimits) -> Self {
        Self { inner, limits }
    }
}

impl<W> FrameWriter<W> {
    #[allow(dead_code)]
    pub fn new(inner: W) -> Self {
        Self::with_limits(inner, TransferLimits::default())
    }

    pub fn with_limits(inner: W, limits: TransferLimits) -> Self {
        Self { inner, limits }
    }
}

impl<R> FrameReader<R>
where
    R: AsyncRead + Unpin,
{
    pub async fn read_frame(&mut self) -> Result<Frame> {
        let raw = self.read_raw_frame().await?;

        match raw.frame_type {
            FRAME_CONTROL => {
                if !raw.data.is_empty() {
                    bail!("control frame unexpectedly carried binary data");
                }
                let message: ControlMessage =
                    decode_payload(&raw.meta, "failed to decode control frame")?;
                Ok(Frame::Control(message))
            }
            FRAME_FILE_CHUNK => {
                let header: FileChunkHeader =
                    decode_payload(&raw.meta, "failed to decode file header")?;
                Ok(Frame::FileChunk(header, raw.data))
            }
            FRAME_CLIPBOARD_META => self.read_clipboard_frame(raw).await,
            FRAME_CLIPBOARD_CHUNK => bail!("unexpected clipboard chunk without clipboard header"),
            other => bail!("unknown frame type {}", other),
        }
    }

    async fn read_raw_frame(&mut self) -> Result<RawFrame> {
        let frame_type = self.inner.read_u8().await?;
        let meta_len = self.inner.read_u32().await? as usize;
        let data_len = self.inner.read_u64().await? as usize;

        ensure_len(
            "incoming frame metadata",
            meta_len,
            self.limits.max_meta_len,
        )?;
        ensure_len(
            "incoming frame data",
            data_len,
            self.limits.max_frame_data_len,
        )?;

        let mut meta = vec![0u8; meta_len];
        self.inner.read_exact(&mut meta).await?;
        let mut data = vec![0u8; data_len];
        self.inner.read_exact(&mut data).await?;

        Ok(RawFrame {
            frame_type,
            meta,
            data,
        })
    }

    async fn read_clipboard_frame(&mut self, raw: RawFrame) -> Result<Frame> {
        if !raw.data.is_empty() {
            bail!("clipboard metadata frame unexpectedly carried binary data");
        }

        let header: ClipboardTransferHeader =
            decode_payload(&raw.meta, "failed to decode clipboard transfer header")?;
        let binary_len = usize::try_from(header.binary_len)
            .context("clipboard binary length overflowed usize")?;
        ensure_len(
            "incoming clipboard binary payload",
            binary_len,
            self.limits.max_clipboard_binary_len,
        )?;

        if binary_len == 0 {
            return Ok(Frame::Clipboard(ClipboardPayload::from_wire(
                header.payload,
                Vec::new(),
            )?));
        }

        let mut binary = Vec::with_capacity(binary_len);
        let mut expected_offset = 0u64;

        loop {
            let chunk = self.read_raw_frame().await?;
            if chunk.frame_type != FRAME_CLIPBOARD_CHUNK {
                bail!("clipboard transfer was interrupted by a non-chunk frame");
            }

            let chunk_header: ClipboardChunkHeader =
                decode_payload(&chunk.meta, "failed to decode clipboard chunk header")?;
            if chunk_header.transfer_id != header.transfer_id {
                bail!("clipboard transfer id mismatch");
            }
            if chunk_header.offset != expected_offset {
                bail!(
                    "clipboard chunk offset mismatch: expected {}, got {}",
                    expected_offset,
                    chunk_header.offset
                );
            }
            if chunk.data.is_empty() {
                bail!("clipboard chunk unexpectedly carried no data");
            }

            expected_offset = expected_offset
                .checked_add(chunk.data.len() as u64)
                .context("clipboard chunk offset overflowed")?;
            if expected_offset > header.binary_len {
                bail!("clipboard transfer exceeded announced length");
            }
            binary.extend_from_slice(&chunk.data);

            if chunk_header.final_chunk {
                if expected_offset != header.binary_len {
                    bail!(
                        "clipboard transfer ended early: expected {}, got {}",
                        header.binary_len,
                        expected_offset
                    );
                }
                return Ok(Frame::Clipboard(ClipboardPayload::from_wire(
                    header.payload,
                    binary,
                )?));
            }
        }
    }
}

impl<W> FrameWriter<W>
where
    W: AsyncWrite + Unpin,
{
    pub async fn write_frame(&mut self, frame: Frame) -> Result<()> {
        match frame {
            Frame::Control(message) => {
                let meta = encode_payload(&message)?;
                self.write_raw_frame(FRAME_CONTROL, &meta, &[]).await?;
            }
            Frame::FileChunk(header, data) => {
                ensure_len(
                    "file chunk data",
                    data.len(),
                    self.limits.max_frame_data_len,
                )?;
                let meta = encode_payload(&header)?;
                self.write_raw_frame(FRAME_FILE_CHUNK, &meta, &data).await?;
            }
            Frame::Clipboard(payload) => {
                let (meta, data) = payload.into_wire()?;
                ensure_len(
                    "clipboard binary payload",
                    data.len(),
                    self.limits.max_clipboard_binary_len,
                )?;
                let transfer_id = Uuid::new_v4();
                let meta = encode_payload(&ClipboardTransferHeader {
                    transfer_id,
                    payload: meta,
                    binary_len: u64::try_from(data.len())
                        .context("clipboard payload length overflowed u64")?,
                })?;
                self.write_raw_frame(FRAME_CLIPBOARD_META, &meta, &[])
                    .await?;

                for (index, chunk) in data.chunks(CLIPBOARD_STREAM_CHUNK_SIZE).enumerate() {
                    let offset = index
                        .checked_mul(CLIPBOARD_STREAM_CHUNK_SIZE)
                        .context("clipboard chunk offset overflowed")?;
                    let meta = encode_payload(&ClipboardChunkHeader {
                        transfer_id,
                        offset: u64::try_from(offset)
                            .context("clipboard chunk offset overflowed u64")?,
                        final_chunk: offset + chunk.len() >= data.len(),
                    })?;
                    self.write_raw_frame(FRAME_CLIPBOARD_CHUNK, &meta, chunk)
                        .await?;
                }
            }
        }
        self.inner.flush().await?;
        Ok(())
    }

    async fn write_raw_frame(&mut self, frame_type: u8, meta: &[u8], data: &[u8]) -> Result<()> {
        ensure_len("frame metadata", meta.len(), self.limits.max_meta_len)?;
        ensure_len("frame data", data.len(), self.limits.max_frame_data_len)?;
        let meta_len = u32::try_from(meta.len()).context("frame metadata length overflowed u32")?;
        let data_len = u64::try_from(data.len()).context("frame data length overflowed u64")?;

        self.inner.write_u8(frame_type).await?;
        self.inner.write_u32(meta_len).await?;
        self.inner.write_u64(data_len).await?;
        self.inner.write_all(meta).await?;
        self.inner.write_all(data).await?;
        Ok(())
    }
}

fn ensure_len(context: &'static str, actual: usize, limit: usize) -> Result<()> {
    if actual > limit {
        return Err(FrameSizeLimitError::new(context, actual, limit).into());
    }
    Ok(())
}

fn append_binary(data: &mut Vec<u8>, bytes: Vec<u8>) -> Result<ClipboardBinaryMeta> {
    let offset = data.len();
    data.extend_from_slice(&bytes);
    let len = bytes.len();
    let offset = u64::try_from(offset).context("clipboard payload offset overflowed u64")?;
    let len = u64::try_from(len).context("clipboard payload length overflowed u64")?;
    Ok(ClipboardBinaryMeta { offset, len })
}

fn read_binary(data: &[u8], meta: ClipboardBinaryMeta) -> Result<&[u8]> {
    let start =
        usize::try_from(meta.offset).context("clipboard payload offset overflowed usize")?;
    let len = usize::try_from(meta.len).context("clipboard payload length overflowed usize")?;
    let end = start
        .checked_add(len)
        .context("clipboard payload range overflowed")?;
    data.get(start..end)
        .with_context(|| format!("clipboard payload range {start}..{end} out of bounds"))
}

#[cfg(test)]
mod tests {
    use super::{
        CLIPBOARD_STREAM_CHUNK_SIZE, ClipboardFile, ClipboardImage, ClipboardPayload,
        ControlMessage, Frame, FrameReader, FrameWriter, SessionAgreement, decode_payload,
        encode_payload,
    };
    use crate::cli::{AudioMode, ClipboardMode, SyncMode};
    use crate::sync::WorkspaceSummary;
    use tokio::io::duplex;

    #[test]
    fn clipboard_payload_roundtrip_preserves_binary_content() {
        let payload = ClipboardPayload {
            text: Some("hello".to_string()),
            rich_text: Some("{\\rtf1 hello}".to_string()),
            html: Some("<b>hello</b>".to_string()),
            image: Some(ClipboardImage {
                png_bytes: vec![1, 2, 3, 4],
            }),
            files: vec![
                ClipboardFile {
                    name: "a.txt".to_string(),
                    bytes: b"alpha".to_vec(),
                },
                ClipboardFile {
                    name: "b.bin".to_string(),
                    bytes: vec![9, 8, 7],
                },
            ],
        };

        let (meta, data) = payload.clone().into_wire().unwrap();
        let decoded = ClipboardPayload::from_wire(meta, data).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn clipboard_payload_rejects_invalid_binary_range() {
        let meta = super::ClipboardPayloadMeta {
            text: None,
            rich_text: None,
            html: None,
            image: Some(super::ClipboardBinaryMeta { offset: 2, len: 5 }),
            files: vec![],
        };

        let err = ClipboardPayload::from_wire(meta, vec![1, 2, 3]).unwrap_err();
        assert!(err.to_string().contains("out of bounds"));
    }

    #[test]
    fn control_message_roundtrip_with_bincode() {
        let message = ControlMessage::PairDecision {
            accepted: true,
            message: "ok".to_string(),
            server: super::DeviceIdentity {
                device_id: uuid::Uuid::nil(),
                device_name: "server".to_string(),
                instance_name: Some("worker-a".to_string()),
                identity_public_key: "pub".to_string(),
                tls_root_certificate: "cert".to_string(),
            },
            workspace: WorkspaceSummary {
                mode: SyncMode::Both,
                send_description: Some("demo".to_string()),
                send_layout: None,
                send_items: vec!["docs".to_string()],
                receive_root: Some("/tmp".to_string()),
                initial_sync: Some(crate::cli::InitialSyncMode::This),
                max_folder_depth: Some(2),
                clipboard_mode: ClipboardMode::Both,
            },
            agreement: SessionAgreement {
                host_to_client: true,
                client_to_host: true,
            },
            audio_mode: AudioMode::Receive,
            auth_method: super::PairAuthMethod::TrustedDevice,
            server_trusts_client: true,
            proof: "proof".to_string(),
            trust_established: true,
        };

        let encoded = encode_payload(&message).unwrap();
        let decoded: ControlMessage =
            decode_payload(&encoded, "failed to decode control frame").unwrap();

        match decoded {
            ControlMessage::PairDecision {
                accepted,
                message,
                server_trusts_client,
                trust_established,
                ..
            } => {
                assert!(accepted);
                assert_eq!(message, "ok");
                assert!(server_trusts_client);
                assert!(trust_established);
            }
            other => panic!("expected pair decision, got {other:?}"),
        }
    }

    #[test]
    fn control_message_roundtrip_overwrite_paused() {
        let message = ControlMessage::OverwritePaused {
            revision: 7,
            paths: vec!["docs/readme.txt".to_string(), "bin/tool".to_string()],
        };

        let encoded = encode_payload(&message).unwrap();
        let decoded: ControlMessage =
            decode_payload(&encoded, "failed to decode control frame").unwrap();

        match decoded {
            ControlMessage::OverwritePaused { revision, paths } => {
                assert_eq!(revision, 7);
                assert_eq!(paths, vec!["docs/readme.txt", "bin/tool"]);
            }
            other => panic!("expected overwrite paused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn clipboard_frame_roundtrip_streams_large_binary_payload() {
        let payload = ClipboardPayload {
            text: Some("stream".to_string()),
            rich_text: None,
            html: None,
            image: Some(ClipboardImage {
                png_bytes: vec![7u8; CLIPBOARD_STREAM_CHUNK_SIZE + 321],
            }),
            files: vec![ClipboardFile {
                name: "big.bin".to_string(),
                bytes: vec![9u8; CLIPBOARD_STREAM_CHUNK_SIZE + 17],
            }],
        };

        let (client, server) = duplex(64 * 1024);
        let expected = payload.clone();

        let writer = tokio::spawn(async move {
            let mut writer = FrameWriter::new(client);
            writer.write_frame(Frame::Clipboard(payload)).await.unwrap();
        });

        let mut reader = FrameReader::new(server);
        let frame = reader.read_frame().await.unwrap();
        writer.await.unwrap();

        match frame {
            Frame::Clipboard(decoded) => assert_eq!(decoded, expected),
            other => panic!("expected clipboard frame, got {other:?}"),
        }
    }
}
