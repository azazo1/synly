use crate::cli::SyncMode;
use crate::sync::{ManifestSnapshot, WorkspaceSummary};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

const FRAME_CONTROL: u8 = 1;
const FRAME_FILE_CHUNK: u8 = 2;
const MAX_META_LEN: usize = 4 * 1024 * 1024;
const MAX_DATA_LEN: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceIdentity {
    pub device_id: Uuid,
    pub device_name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairRequestPayload {
    pub protocol_version: u16,
    pub client: DeviceIdentity,
    pub requested_mode: SyncMode,
    pub workspace: WorkspaceSummary,
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
    PairRequest {
        payload: PairRequestPayload,
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
        proof: String,
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
    ClipboardUpdate {
        text: String,
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

#[derive(Clone, Debug)]
pub enum Frame {
    Control(ControlMessage),
    FileChunk(FileChunkHeader, Vec<u8>),
}

pub struct FrameReader<R> {
    inner: R,
}

pub struct FrameWriter<W> {
    inner: W,
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
        }
        self.inner.flush().await?;
        Ok(())
    }
}
