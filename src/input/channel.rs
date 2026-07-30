use super::protocol::{InputMessage, read_message, write_message};
use anyhow::{Context, Result, bail};
use hmac::{Hmac, Mac};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};
use uuid::Uuid;

pub const INPUT_PREAMBLE_MAGIC: &[u8; 12] = b"SYNLY-INPUT\0";
pub const INPUT_AUX_VERSION: u16 = 1;
const INPUT_ALPN: &[u8] = b"synly-input/1";
const INPUT_EXPORTER_LABEL: &[u8] = b"synly/input/aux/v1";
const INPUT_PROOF_LABEL: &[u8] = b"synly-input-proof-v1";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputChannelRole {
    Host,
    Client,
}

impl InputChannelRole {
    fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::Host => b"host",
            Self::Client => b"client",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputChannelOffer {
    pub session_id: Uuid,
    pub certificate_der: Vec<u8>,
}

pub struct InputHostChannel {
    offer: InputChannelOffer,
    acceptor: TlsAcceptor,
}

impl InputHostChannel {
    pub fn create() -> Result<Self> {
        let certified = generate_simple_self_signed(vec!["synly.local".to_string()])?;
        let certificate_der = certified.cert.der().to_vec();
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            certified.signing_key.serialize_der(),
        ));
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(certificate_der.clone())],
                private_key,
            )?;
        config.alpn_protocols = vec![INPUT_ALPN.to_vec()];
        let channel = Self {
            offer: InputChannelOffer {
                session_id: Uuid::new_v4(),
                certificate_der,
            },
            acceptor: TlsAcceptor::from(Arc::new(config)),
        };
        tracing::trace!(session_id = %channel.offer.session_id, "输入辅助 host channel 已创建"); // to remove
        Ok(channel)
    }

    pub fn offer(&self) -> &InputChannelOffer {
        &self.offer
    }

    pub async fn accept(
        &self,
        mut socket: TcpStream,
        master_secret: &[u8; 32],
    ) -> Result<TlsStream<TcpStream>> {
        tracing::trace!(session_id = %self.offer.session_id, "输入辅助 host 开始读取 TCP 前导"); // to remove
        socket.set_nodelay(true)?;
        let session_id = read_preamble(&mut socket).await?;
        tracing::trace!(session_id = %session_id, expected_session_id = %self.offer.session_id, "输入辅助 host 已读取 TCP 前导"); // to remove
        self.accept_after_preamble(socket, session_id, master_secret)
            .await
    }

    pub async fn accept_after_preamble(
        &self,
        socket: TcpStream,
        session_id: Uuid,
        master_secret: &[u8; 32],
    ) -> Result<TlsStream<TcpStream>> {
        socket.set_nodelay(true)?;
        if session_id != self.offer.session_id {
            tracing::trace!(session_id = %session_id, expected_session_id = %self.offer.session_id, "输入辅助 host session id 校验失败"); // to remove
            bail!("输入辅助连接 session_id 不匹配");
        }
        tracing::trace!(session_id = %session_id, "输入辅助 host 开始 TLS 握手"); // to remove
        let stream = self.acceptor.accept(socket).await?;
        tracing::trace!(session_id = %session_id, "输入辅助 host TLS 握手完成"); // to remove
        let exporter = export_server(&stream, self.offer.session_id)?;
        tracing::trace!(session_id = %session_id, "输入辅助 host 已导出 TLS channel binding"); // to remove
        let mut stream: TlsStream<TcpStream> = stream.into();
        let message = read_message(&mut stream).await?;
        let InputMessage::Proof { role, proof } = message else {
            tracing::trace!(session_id = %session_id, "输入辅助 host 收到非 proof 首帧"); // to remove
            bail!("输入辅助连接缺少客户端证明");
        };
        tracing::trace!(session_id = %session_id, role = ?role, "输入辅助 host 已读取客户端 proof"); // to remove
        if role != InputChannelRole::Client {
            tracing::trace!(session_id = %session_id, role = ?role, "输入辅助 host 客户端 proof role 校验失败"); // to remove
            bail!("输入辅助连接角色不正确");
        }
        if let Err(error) = verify_proof(
            master_secret,
            self.offer.session_id,
            role,
            &exporter,
            &proof,
        ) {
            tracing::trace!(session_id = %session_id, error = %error, "输入辅助 host 客户端 proof 校验失败"); // to remove
            return Err(error);
        }
        tracing::trace!(session_id = %session_id, "输入辅助 host 客户端 proof 校验通过"); // to remove
        write_message(
            &mut stream,
            &InputMessage::Proof {
                role: InputChannelRole::Host,
                proof: make_proof(
                    master_secret,
                    self.offer.session_id,
                    InputChannelRole::Host,
                    &exporter,
                )?,
            },
        )
        .await?;
        tracing::trace!(session_id = %session_id, "输入辅助 host 已写入服务端 proof"); // to remove
        Ok(stream)
    }
}

pub async fn connect(
    address: std::net::SocketAddr,
    offer: &InputChannelOffer,
    master_secret: &[u8; 32],
) -> Result<TlsStream<TcpStream>> {
    tracing::trace!(%address, session_id = %offer.session_id, "输入辅助 client 开始 TCP 连接"); // to remove
    if offer.certificate_der.is_empty() || offer.certificate_der.len() > 64 * 1024 {
        tracing::trace!(session_id = %offer.session_id, certificate_len = offer.certificate_der.len(), "输入辅助 client 证书长度校验失败"); // to remove
        bail!("输入辅助证书长度无效");
    }
    let mut socket = TcpStream::connect(address)
        .await
        .with_context(|| format!("无法连接输入辅助通道 {address}"))?;
    socket.set_nodelay(true)?;
    write_preamble(&mut socket, offer.session_id).await?;
    tracing::trace!(%address, session_id = %offer.session_id, "输入辅助 client 已写入 TCP 前导"); // to remove

    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(offer.certificate_der.clone()))?;
    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![INPUT_ALPN.to_vec()];
    let connector = TlsConnector::from(Arc::new(config));
    let stream = connector
        .connect(input_server_name()?, socket)
        .await?;
    tracing::trace!(%address, session_id = %offer.session_id, "输入辅助 client TLS 握手完成"); // to remove
    let exporter = export_client(&stream, offer.session_id)?;
    tracing::trace!(session_id = %offer.session_id, "输入辅助 client 已导出 TLS channel binding"); // to remove
    let mut stream: TlsStream<TcpStream> = stream.into();
    write_message(
        &mut stream,
        &InputMessage::Proof {
            role: InputChannelRole::Client,
            proof: make_proof(
                master_secret,
                offer.session_id,
                InputChannelRole::Client,
                &exporter,
            )?,
        },
    )
    .await?;
    tracing::trace!(session_id = %offer.session_id, "输入辅助 client 已写入客户端 proof"); // to remove
    let message = read_message(&mut stream).await?;
    let InputMessage::Proof { role, proof } = message else {
        tracing::trace!(session_id = %offer.session_id, "输入辅助 client 收到非 proof 首帧"); // to remove
        bail!("输入辅助连接缺少服务端证明");
    };
    tracing::trace!(session_id = %offer.session_id, role = ?role, "输入辅助 client 已读取服务端 proof"); // to remove
    if role != InputChannelRole::Host {
        tracing::trace!(session_id = %offer.session_id, role = ?role, "输入辅助 client 服务端 proof role 校验失败"); // to remove
        bail!("输入辅助连接服务端角色不正确");
    }
    if let Err(error) = verify_proof(
        master_secret,
        offer.session_id,
        role,
        &exporter,
        &proof,
    ) {
        tracing::trace!(session_id = %offer.session_id, error = %error, "输入辅助 client 服务端 proof 校验失败"); // to remove
        return Err(error);
    }
    tracing::trace!(session_id = %offer.session_id, "输入辅助 client 服务端 proof 校验通过"); // to remove
    Ok(stream)
}

fn input_server_name() -> Result<ServerName<'static>> {
    Ok(ServerName::try_from("synly.local")?.to_owned())
}

pub async fn write_preamble<W>(socket: &mut W, session_id: Uuid) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    tracing::trace!(session_id = %session_id, "输入辅助写入 TCP 前导"); // to remove
    socket.write_all(INPUT_PREAMBLE_MAGIC).await?;
    socket.write_u16(INPUT_AUX_VERSION).await?;
    socket.write_all(session_id.as_bytes()).await?;
    socket.flush().await?;
    tracing::trace!(session_id = %session_id, "输入辅助 TCP 前导写入完成"); // to remove
    Ok(())
}

pub async fn read_preamble<R>(socket: &mut R) -> Result<Uuid>
where
    R: tokio::io::AsyncRead + Unpin,
{
    tracing::trace!("输入辅助开始读取 TCP 前导"); // to remove
    let mut magic = [0u8; INPUT_PREAMBLE_MAGIC.len()];
    socket.read_exact(&mut magic).await?;
    if &magic != INPUT_PREAMBLE_MAGIC {
        tracing::trace!(magic = ?magic, "输入辅助 TCP 前导 magic 校验失败"); // to remove
        bail!("输入辅助连接前导标识无效");
    }
    let version = socket.read_u16().await?;
    if version != INPUT_AUX_VERSION {
        tracing::trace!(version, expected_version = INPUT_AUX_VERSION, "输入辅助 TCP 前导版本校验失败"); // to remove
        bail!("不支持的输入辅助协议版本: {version}");
    }
    let mut session_id = [0u8; 16];
    socket.read_exact(&mut session_id).await?;
    let session_id = Uuid::from_bytes(session_id);
    tracing::trace!(%session_id, version, "输入辅助 TCP 前导读取完成"); // to remove
    Ok(session_id)
}

fn export_client(
    stream: &tokio_rustls::client::TlsStream<TcpStream>,
    session_id: Uuid,
) -> Result<[u8; 32]> {
    let mut output = [0u8; 32];
    stream.get_ref().1.export_keying_material(
        &mut output,
        INPUT_EXPORTER_LABEL,
        Some(session_id.as_bytes()),
    )?;
    Ok(output)
}

fn export_server(
    stream: &tokio_rustls::server::TlsStream<TcpStream>,
    session_id: Uuid,
) -> Result<[u8; 32]> {
    let mut output = [0u8; 32];
    stream.get_ref().1.export_keying_material(
        &mut output,
        INPUT_EXPORTER_LABEL,
        Some(session_id.as_bytes()),
    )?;
    Ok(output)
}

fn make_proof(
    master_secret: &[u8; 32],
    session_id: Uuid,
    role: InputChannelRole,
    exporter: &[u8; 32],
) -> Result<[u8; 32]> {
    let mut mac = Hmac::<Sha256>::new_from_slice(master_secret)?;
    mac.update(INPUT_PROOF_LABEL);
    mac.update(session_id.as_bytes());
    mac.update(role.as_bytes());
    mac.update(exporter);
    Ok(mac.finalize().into_bytes().into())
}

fn verify_proof(
    master_secret: &[u8; 32],
    session_id: Uuid,
    role: InputChannelRole,
    exporter: &[u8; 32],
    proof: &[u8; 32],
) -> Result<()> {
    let mut mac = Hmac::<Sha256>::new_from_slice(master_secret)?;
    mac.update(INPUT_PROOF_LABEL);
    mac.update(session_id.as_bytes());
    mac.update(role.as_bytes());
    mac.update(exporter);
    mac.verify_slice(proof)
        .map_err(|_| anyhow::anyhow!("输入辅助连接 HMAC 证明无效"))
}

#[cfg(test)]
mod tests {
    use super::{
        InputChannelRole, InputHostChannel, connect, make_proof, read_preamble, verify_proof,
        write_preamble,
    };
    use crate::input::protocol::{InputMessage, read_message, write_message};
    use crate::input::{KeySnapshot, ModifierMask, ScreenEdge};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::io::duplex;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;
    use tokio::time::{self, Duration, MissedTickBehavior};
    use uuid::Uuid;

    #[test]
    fn proof_binds_secret_session_role_and_exporter() {
        let secret = [1u8; 32];
        let session = Uuid::new_v4();
        let exporter = [2u8; 32];
        let proof = make_proof(&secret, session, InputChannelRole::Client, &exporter).unwrap();
        verify_proof(&secret, session, InputChannelRole::Client, &exporter, &proof).unwrap();
        assert!(verify_proof(&[3u8; 32], session, InputChannelRole::Client, &exporter, &proof).is_err());
        assert!(verify_proof(&secret, session, InputChannelRole::Host, &exporter, &proof).is_err());
    }

    #[tokio::test]
    async fn preamble_roundtrip_preserves_session_id() {
        let session_id = Uuid::new_v4();
        let (mut writer, mut reader) = duplex(64);
        let task = tokio::spawn(async move { write_preamble(&mut writer, session_id).await });
        assert_eq!(read_preamble(&mut reader).await.unwrap(), session_id);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn auxiliary_tls_roundtrip_binds_primary_secret() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let channel = InputHostChannel::create().unwrap();
        let offer = channel.offer().clone();
        let secret = [7u8; 32];
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            channel.accept(socket, &secret).await
        });
        let client = connect(address, &offer, &secret).await;
        assert!(client.is_ok());
        assert!(server.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn auxiliary_tls_keeps_bidirectional_frames_flowing_after_activation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let channel = InputHostChannel::create().unwrap();
        let offer = channel.offer().clone();
        let secret = [8u8; 32];
        let (heartbeat_read_tx, heartbeat_read_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let stream = channel.accept(socket, &secret).await.unwrap();
            let (mut reader, mut writer) = tokio::io::split(stream);
            let generation = Arc::new(AtomicU64::new(0));
            let writer_generation = Arc::clone(&generation);
            let writer_task = tokio::spawn(async move {
                let mut heartbeat = time::interval(Duration::from_millis(10));
                heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
                loop {
                    heartbeat.tick().await;
                    write_message(
                        &mut writer,
                        &InputMessage::Heartbeat {
                            generation: writer_generation.load(Ordering::Acquire),
                        },
                    )
                    .await?;
                }
                #[allow(unreachable_code)]
                anyhow::Result::<()>::Ok(())
            });

            let mut motion_count = 0usize;
            while motion_count < 400 {
                match read_message(&mut reader).await.unwrap() {
                    InputMessage::Activate {
                        generation: incoming_generation,
                        ..
                    } => generation.store(incoming_generation, Ordering::Release),
                    InputMessage::Motion { generation: 1, .. } => {
                        motion_count += 1;
                    }
                    _ => {}
                }
            }
            heartbeat_read_rx.await.unwrap();
            writer_task.abort();
            let _ = writer_task.await;
        });

        let stream = connect(address, &offer, &secret).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        write_message(
            &mut writer,
            &InputMessage::Activate {
                generation: 1,
                source_edge: ScreenEdge::Right,
                edge_position: 0.5,
                pressed: KeySnapshot {
                    usages: Vec::new(),
                    modifiers: ModifierMask::default(),
                    buttons: Vec::new(),
                },
            },
        )
        .await
        .unwrap();
        for _ in 0..400 {
            write_message(
                &mut writer,
                &InputMessage::Motion {
                    generation: 1,
                    dx: 1,
                    dy: 0,
                },
            )
            .await
            .unwrap();
        }
        time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    read_message(&mut reader).await.unwrap(),
                    InputMessage::Heartbeat { generation: 1 }
                ) {
                    break;
                }
            }
        })
        .await
        .expect("辅助 TLS 通道应在持续运动后返回新 generation 心跳");
        heartbeat_read_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn auxiliary_tls_recovers_after_stale_preamble() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let channel = InputHostChannel::create().unwrap();
        let offer = channel.offer().clone();
        let expected_session_id = offer.session_id;
        let secret = [9u8; 32];
        let server = tokio::spawn(async move {
            let (mut stale, _) = listener.accept().await.unwrap();
            let stale_session_id = read_preamble(&mut stale).await.unwrap();
            assert_ne!(stale_session_id, expected_session_id);
            drop(stale);

            let (mut socket, _) = listener.accept().await.unwrap();
            let session_id = read_preamble(&mut socket).await.unwrap();
            channel
                .accept_after_preamble(socket, session_id, &secret)
                .await
        });
        let mut stale = TcpStream::connect(address).await.unwrap();
        write_preamble(&mut stale, Uuid::nil()).await.unwrap();
        drop(stale);

        assert!(connect(address, &offer, &secret).await.is_ok());
        assert!(server.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn auxiliary_tls_rejects_wrong_primary_secret() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let channel = InputHostChannel::create().unwrap();
        let offer = channel.offer().clone();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            channel.accept(socket, &[1u8; 32]).await
        });
        assert!(connect(address, &offer, &[2u8; 32]).await.is_err());
        assert!(server.await.unwrap().is_err());
    }
}
