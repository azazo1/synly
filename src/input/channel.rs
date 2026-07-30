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
        Ok(Self {
            offer: InputChannelOffer {
                session_id: Uuid::new_v4(),
                certificate_der,
            },
            acceptor: TlsAcceptor::from(Arc::new(config)),
        })
    }

    pub fn offer(&self) -> &InputChannelOffer {
        &self.offer
    }

    pub async fn accept(
        &self,
        mut socket: TcpStream,
        master_secret: &[u8; 32],
    ) -> Result<TlsStream<TcpStream>> {
        socket.set_nodelay(true)?;
        let session_id = read_preamble(&mut socket).await?;
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
            bail!("输入辅助连接 session_id 不匹配");
        }
        let stream = self.acceptor.accept(socket).await?;
        let exporter = export_server(&stream, self.offer.session_id)?;
        let mut stream: TlsStream<TcpStream> = stream.into();
        let message = read_message(&mut stream).await?;
        let InputMessage::Proof { role, proof } = message else {
            bail!("输入辅助连接缺少客户端证明");
        };
        if role != InputChannelRole::Client {
            bail!("输入辅助连接角色不正确");
        }
        verify_proof(
            master_secret,
            self.offer.session_id,
            role,
            &exporter,
            &proof,
        )?;
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
        Ok(stream)
    }
}

pub async fn connect(
    address: std::net::SocketAddr,
    offer: &InputChannelOffer,
    master_secret: &[u8; 32],
) -> Result<TlsStream<TcpStream>> {
    if offer.certificate_der.is_empty() || offer.certificate_der.len() > 64 * 1024 {
        bail!("输入辅助证书长度无效");
    }
    let mut socket = TcpStream::connect(address)
        .await
        .with_context(|| format!("无法连接输入辅助通道 {address}"))?;
    socket.set_nodelay(true)?;
    write_preamble(&mut socket, offer.session_id).await?;

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
    let exporter = export_client(&stream, offer.session_id)?;
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
    let message = read_message(&mut stream).await?;
    let InputMessage::Proof { role, proof } = message else {
        bail!("输入辅助连接缺少服务端证明");
    };
    if role != InputChannelRole::Host {
        bail!("输入辅助连接服务端角色不正确");
    }
    verify_proof(
        master_secret,
        offer.session_id,
        role,
        &exporter,
        &proof,
    )?;
    Ok(stream)
}

fn input_server_name() -> Result<ServerName<'static>> {
    Ok(ServerName::try_from("synly.local")?.to_owned())
}

pub async fn write_preamble<W>(socket: &mut W, session_id: Uuid) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    socket.write_all(INPUT_PREAMBLE_MAGIC).await?;
    socket.write_u16(INPUT_AUX_VERSION).await?;
    socket.write_all(session_id.as_bytes()).await?;
    socket.flush().await?;
    Ok(())
}

pub async fn read_preamble<R>(socket: &mut R) -> Result<Uuid>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut magic = [0u8; INPUT_PREAMBLE_MAGIC.len()];
    socket.read_exact(&mut magic).await?;
    if &magic != INPUT_PREAMBLE_MAGIC {
        bail!("输入辅助连接前导标识无效");
    }
    let version = socket.read_u16().await?;
    if version != INPUT_AUX_VERSION {
        bail!("不支持的输入辅助协议版本: {version}");
    }
    let mut session_id = [0u8; 16];
    socket.read_exact(&mut session_id).await?;
    Ok(Uuid::from_bytes(session_id))
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
    use super::{InputChannelRole, InputHostChannel, connect, make_proof, read_preamble, verify_proof, write_preamble};
    use tokio::io::duplex;
    use tokio::net::{TcpListener, TcpStream};
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
