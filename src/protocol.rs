use crate::cli::SyncMode;
use crate::sync::{ManifestSnapshot, WorkspaceSummary};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

const FRAME_CONTROL: u8 = 1;
const FRAME_FILE_CHUNK: u8 = 2;
const FRAME_CLIPBOARD: u8 = 3;
const MAX_META_LEN: usize = 4 * 1024 * 1024;
const MAX_DATA_LEN: usize = 64 * 1024 * 1024;

pub const PROTOCOL_VERSION: u16 = 6;

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
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlMessage {
    BootstrapHello {
        protocol_version: u16,
        client_bootstrap_public_key: String,
    },
    BootstrapChallenge {
        request_id: String,
        server_bootstrap_public_key: String,
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
        auth_method: PairAuthMethod,
        proof: String,
        #[serde(default)]
        trust_established: bool,
    },
    SnapshotAdvert {
        revision: u64,
        snapshot: ManifestSnapshot,
    },
    FileRequest {
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
}

pub struct FrameWriter<W> {
    inner: W,
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

impl<R> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }
}

impl<W> FrameWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }
}

impl<R> FrameReader<R>
where
    R: AsyncRead + Unpin,
{
    pub async fn read_frame(&mut self) -> Result<Frame> {
        let frame_type = self.inner.read_u8().await?;
        let meta_len = self.inner.read_u32().await? as usize;
        let data_len = self.inner.read_u64().await? as usize;

        if meta_len > MAX_META_LEN {
            bail!("frame metadata too large: {}", meta_len);
        }
        if data_len > MAX_DATA_LEN {
            bail!("frame data too large: {}", data_len);
        }

        let mut meta = vec![0u8; meta_len];
        self.inner.read_exact(&mut meta).await?;

        match frame_type {
            FRAME_CONTROL => {
                if data_len != 0 {
                    bail!("control frame unexpectedly carried binary data");
                }
                let message: ControlMessage =
                    serde_json::from_slice(&meta).context("failed to decode control frame")?;
                Ok(Frame::Control(message))
            }
            FRAME_FILE_CHUNK => {
                let header: FileChunkHeader =
                    serde_json::from_slice(&meta).context("failed to decode file header")?;
                let mut data = vec![0u8; data_len];
                self.inner.read_exact(&mut data).await?;
                Ok(Frame::FileChunk(header, data))
            }
            FRAME_CLIPBOARD => {
                let payload_meta: ClipboardPayloadMeta = serde_json::from_slice(&meta)
                    .context("failed to decode clipboard frame metadata")?;
                let mut data = vec![0u8; data_len];
                self.inner.read_exact(&mut data).await?;
                Ok(Frame::Clipboard(ClipboardPayload::from_wire(
                    payload_meta,
                    data,
                )?))
            }
            other => bail!("unknown frame type {}", other),
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
                let meta = serde_json::to_vec(&message)?;
                self.inner.write_u8(FRAME_CONTROL).await?;
                self.inner.write_u32(meta.len() as u32).await?;
                self.inner.write_u64(0).await?;
                self.inner.write_all(&meta).await?;
            }
            Frame::FileChunk(header, data) => {
                if data.len() > MAX_DATA_LEN {
                    bail!("refusing to send over-sized file chunk: {}", data.len());
                }
                let meta = serde_json::to_vec(&header)?;
                self.inner.write_u8(FRAME_FILE_CHUNK).await?;
                self.inner.write_u32(meta.len() as u32).await?;
                self.inner.write_u64(data.len() as u64).await?;
                self.inner.write_all(&meta).await?;
                self.inner.write_all(&data).await?;
            }
            Frame::Clipboard(payload) => {
                let (meta, data) = payload.into_wire()?;
                if data.len() > MAX_DATA_LEN {
                    bail!(
                        "refusing to send over-sized clipboard payload: {}",
                        data.len()
                    );
                }
                let meta = serde_json::to_vec(&meta)?;
                self.inner.write_u8(FRAME_CLIPBOARD).await?;
                self.inner.write_u32(meta.len() as u32).await?;
                self.inner.write_u64(data.len() as u64).await?;
                self.inner.write_all(&meta).await?;
                self.inner.write_all(&data).await?;
            }
        }
        self.inner.flush().await?;
        Ok(())
    }
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
    use super::{ClipboardFile, ClipboardImage, ClipboardPayload};

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
}
