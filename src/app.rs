use crate::cli::{
    ConnectionPreference, RuntimeOptions, SyncMode, prompt_confirm, prompt_select,
    require_peer_query, resolve_pairing_pin, sync_clipboard_label, sync_delete_label,
};
use crate::clipboard::ClipboardSync;
use crate::config::{DeviceConfig, SynlyConfig, TrustedDeviceConfig};
use crate::crypto;
use crate::discovery::{self, Advertisement, DiscoveredPeer};
use crate::protocol::{
    ClipboardPayload, ControlMessage, DeviceIdentity, FileChunkHeader, Frame, FrameReader,
    FrameWriter, PROTOCOL_VERSION, PairAuthMethod, PairRequestPayload, SessionAgreement,
};
use crate::sync::{
    DeletePolicy, WorkspaceSpec, apply_file_metadata, build_apply_plan, build_incoming_snapshot,
    build_snapshot, delete_paths_best_effort, ensure_directories,
    filter_snapshot_for_incoming_root, resolve_incoming_path, resolve_outgoing_path,
    snapshot_contains_file, watch_targets,
};
use anyhow::{Context, Result, anyhow, bail};
use console::style;
use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};
use rand::RngExt;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{self, Instant};
use tokio_rustls::TlsStream;
use uuid::Uuid;

const FILE_CHUNK_SIZE: usize = 256 * 1024;
const PAIRING_TIMEOUT: Duration = Duration::from_secs(90);
const TLS_UPGRADE_TIMEOUT: Duration = Duration::from_secs(15);
const PAIRING_FAILURE_WINDOW: Duration = Duration::from_secs(5 * 60);
const PAIRING_COOLDOWN: Duration = Duration::from_secs(3 * 60);
const PAIRING_MAX_FAILURES: u32 = 5;
const PAIRING_BACKOFF_BASE_MS: u64 = 1_000;

#[derive(Clone, Copy, Debug)]
enum SessionRole {
    Host,
    Client,
}

#[derive(Debug)]
struct AuthenticatedSession {
    role: SessionRole,
    stream: TlsStream<TcpStream>,
    remote: DeviceIdentity,
    agreement: SessionAgreement,
    remote_workspace: crate::sync::WorkspaceSummary,
}

#[derive(Debug)]
struct PendingRevision {
    requested_files: usize,
    remaining_files: BTreeSet<String>,
    failed_files: BTreeSet<String>,
    delete_paths: Vec<String>,
    transfer_done: bool,
}

struct IncomingFileState {
    file: File,
    temp_path: PathBuf,
    final_path: PathBuf,
    modified_ms: u64,
    executable: bool,
    expected_size: u64,
    written: u64,
}

struct PairDecisionParams<'a> {
    exporter: &'a [u8],
    request_id: &'a str,
    accepted: bool,
    message: String,
    device: &'a DeviceConfig,
    workspace: &'a WorkspaceSpec,
    sync_clipboard: bool,
    agreement: &'a SessionAgreement,
    auth_method: PairAuthMethod,
    pin: Option<&'a str>,
    trust_established: bool,
}

#[derive(Default)]
struct PairingThrottle {
    peers: HashMap<String, PairingPeerState>,
}

struct PairingPeerState {
    failures: u32,
    window_started_at: Instant,
    blocked_until: Option<Instant>,
}

pub async fn run(config: &mut SynlyConfig, options: RuntimeOptions) -> Result<()> {
    match options.connection {
        ConnectionPreference::Host => run_host(config, options).await,
        ConnectionPreference::Join => run_client(config, options).await,
    }
}

async fn run_host(config: &mut SynlyConfig, options: RuntimeOptions) -> Result<()> {
    let device = config.device.clone();
    let mut pairing_throttle = PairingThrottle::default();
    let listener = TcpListener::bind(("0.0.0.0", 0))
        .await
        .context("failed to bind TCP listener")?;
    let port = listener.local_addr()?.port();
    let _advertisement = discovery::advertise(&Advertisement {
        port,
        device: device.clone(),
        mode: options.mode,
    })?;

    print_host_ready(&device, &options, port);

    loop {
        let (socket, address) = listener.accept().await?;
        match handle_incoming_connection(socket, address, &mut pairing_throttle, config, &options)
            .await
        {
            Ok(Some(session)) => {
                run_sync_session(
                    session,
                    &options.workspace,
                    options.interval_secs,
                    options.sync_delete,
                    options.sync_clipboard,
                    &options.clipboard,
                )
                .await?;
                break;
            }
            Ok(None) => continue,
            Err(err) => {
                eprintln!("连接失败: {err:#}");
            }
        }
    }

    Ok(())
}

async fn run_client(config: &mut SynlyConfig, options: RuntimeOptions) -> Result<()> {
    loop {
        let peer = choose_peer(
            options.pairing.peer_query.as_deref(),
            Duration::from_secs(options.pairing.discovery_secs),
        )?;
        match connect_to_peer(&peer, config, &options).await {
            Ok(session) => {
                run_sync_session(
                    session,
                    &options.workspace,
                    options.interval_secs,
                    options.sync_delete,
                    options.sync_clipboard,
                    &options.clipboard,
                )
                .await?;
                break;
            }
            Err(err) => {
                eprintln!("连接失败: {err:#}");
                if options.pairing.peer_query.is_some() {
                    break;
                }
                if !prompt_confirm("重新搜索设备吗", true)? {
                    break;
                }
            }
        }
    }

    Ok(())
}

async fn handle_incoming_connection(
    socket: TcpStream,
    remote_addr: SocketAddr,
    pairing_throttle: &mut PairingThrottle,
    config: &mut SynlyConfig,
    options: &RuntimeOptions,
) -> Result<Option<AuthenticatedSession>> {
    let mut first_byte = [0u8; 1];
    let peeked = socket.peek(&mut first_byte).await?;
    if peeked == 0 {
        return Ok(None);
    }

    if first_byte[0] == 0x16 {
        handle_trusted_incoming_connection(socket, remote_addr.to_string(), config, options).await
    } else {
        handle_bootstrap_incoming_connection(socket, remote_addr, pairing_throttle, config, options)
            .await
    }
}

async fn connect_to_peer(
    peer: &DiscoveredPeer,
    config: &mut SynlyConfig,
    options: &RuntimeOptions,
) -> Result<AuthenticatedSession> {
    let device = config.device.clone();
    let address = peer
        .addresses
        .first()
        .copied()
        .context("peer advertised no IPv4 address")?;
    let trusted_device = trusted_device_for_peer(config, peer)?;
    let trusted_transport = trusted_device
        .as_ref()
        .filter(|device| !device.tls_root_certificate.trim().is_empty());
    if options.pairing.trusted_only && trusted_transport.is_none() {
        bail!("目标设备尚未建立完整的可信 mTLS 信任，请先用一次 PIN 配对并加上 --trust-device");
    }
    match trusted_transport {
        Some(trusted_device) => {
            connect_to_trusted_peer(address, peer.port, &device, trusted_device, config, options)
                .await
        }
        None => connect_to_untrusted_peer(address, peer.port, &device, config, options).await,
    }
}

async fn handle_trusted_incoming_connection(
    socket: TcpStream,
    remote_addr: String,
    config: &mut SynlyConfig,
    options: &RuntimeOptions,
) -> Result<Option<AuthenticatedSession>> {
    let device = config.device.clone();
    if !has_trusted_transport(config) {
        bail!("收到 TLS 连接，但本机尚未保存任何可信设备根证书；未信任设备必须先走 bootstrap/PIN");
    }

    let acceptor = crypto::build_server_acceptor(&device, &config.trusted_devices)?;
    let mut server_stream = acceptor.accept(socket).await?;
    let frame = FrameReader::new(&mut server_stream).read_frame().await?;
    let (request_id, payload, trusted_proof) = match frame {
        Frame::Control(ControlMessage::PairRequest {
            request_id,
            payload,
            trusted_proof,
        }) => (request_id, payload, trusted_proof),
        _ => {
            FrameWriter::new(&mut server_stream)
                .write_frame(Frame::Control(ControlMessage::Error {
                    message: "连接建立了，但请求格式不正确".to_string(),
                }))
                .await?;
            return Ok(None);
        }
    };

    if payload.protocol_version != PROTOCOL_VERSION {
        FrameWriter::new(&mut server_stream)
            .write_frame(Frame::Control(ControlMessage::Error {
                message: format!("不支持的协议版本: {}", payload.protocol_version),
            }))
            .await?;
        return Ok(None);
    }

    if let Err(err) = crypto::verify_device_identity_material(&payload.client) {
        FrameWriter::new(&mut server_stream)
            .write_frame(Frame::Control(ControlMessage::Error {
                message: format!("对端提供的设备身份材料无效: {err:#}"),
            }))
            .await?;
        return Ok(None);
    }

    let trusted_device = match config.trusted_device(&payload.client.device_id).cloned() {
        Some(trusted_device) => trusted_device,
        None => {
            FrameWriter::new(&mut server_stream)
                .write_frame(Frame::Control(ControlMessage::Error {
                    message: "该设备未处于可信状态，不能走免 PIN 的 mTLS 直连。".to_string(),
                }))
                .await?;
            return Ok(None);
        }
    };
    let Some(trusted_proof) = trusted_proof.as_deref() else {
        FrameWriter::new(&mut server_stream)
            .write_frame(Frame::Control(ControlMessage::Error {
                message: "可信设备已建立 mTLS，但缺少应用层身份签名。".to_string(),
            }))
            .await?;
        return Ok(None);
    };

    let exporter = crypto::export_keying_material_from_server(&server_stream, &request_id)?;
    if let Err(err) = crypto::verify_device_identity(&payload.client, &trusted_device.public_key)
        .and_then(|_| {
            crypto::verify_trusted_pair_auth(
                &exporter,
                &trusted_device.public_key,
                &request_id,
                &payload,
                trusted_proof,
            )
        })
    {
        FrameWriter::new(&mut server_stream)
            .write_frame(Frame::Control(ControlMessage::Error {
                message: format!("可信设备身份绑定失败，已拒绝本次连接: {err:#}"),
            }))
            .await?;
        return Ok(None);
    }

    let agreement = negotiate(options.mode, payload.requested_mode);
    print_pair_request_overview(&payload, options, &agreement, &remote_addr)?;
    if !agreement.any_direction() {
        FrameWriter::new(&mut server_stream)
            .write_frame(Frame::Control(ControlMessage::Error {
                message: "双方模式不兼容，本次请求无法建立同步。".to_string(),
            }))
            .await?;
        return Ok(None);
    }

    println!("可信设备 mTLS 与身份签名校验通过。");
    let accepted = if options.pairing.accept {
        true
    } else {
        prompt_confirm("接受这次同步吗", true)?
    };
    let message = if accepted {
        "服务端已接受同步请求。".to_string()
    } else {
        "服务端拒绝了本次同步请求。".to_string()
    };
    let control = signed_pair_decision(PairDecisionParams {
        exporter: &exporter,
        request_id: &request_id,
        accepted,
        message,
        device: &device,
        workspace: &options.workspace,
        sync_clipboard: options.sync_clipboard,
        agreement: &agreement,
        auth_method: PairAuthMethod::TrustedDevice,
        pin: None,
        trust_established: false,
    })?;
    FrameWriter::new(&mut server_stream)
        .write_frame(Frame::Control(control))
        .await?;

    if !accepted {
        return Ok(None);
    }

    config.note_trusted_device_session(payload.client.device_id, &payload.client.device_name);
    config.save()?;

    let tls_stream: TlsStream<TcpStream> = server_stream.into();
    Ok(Some(AuthenticatedSession {
        role: SessionRole::Host,
        stream: tls_stream,
        remote: payload.client,
        agreement,
        remote_workspace: payload.workspace,
    }))
}

async fn handle_bootstrap_incoming_connection(
    mut socket: TcpStream,
    remote_addr: SocketAddr,
    pairing_throttle: &mut PairingThrottle,
    config: &mut SynlyConfig,
    options: &RuntimeOptions,
) -> Result<Option<AuthenticatedSession>> {
    let remote_label = remote_addr.to_string();
    let remote_peer_key = remote_addr.ip().to_string();
    if options.pairing.trusted_only {
        FrameWriter::new(&mut socket)
            .write_frame(Frame::Control(ControlMessage::Error {
                message: "当前 host 只允许已建立长期信任的设备通过 mTLS 直连。".to_string(),
            }))
            .await?;
        return Ok(None);
    }

    if let Some(remaining) = pairing_throttle.blocked_remaining(&remote_peer_key) {
        FrameWriter::new(&mut socket)
            .write_frame(Frame::Control(ControlMessage::Error {
                message: format!(
                    "该地址近期配对失败过多，请等待 {} 秒后再试。",
                    remaining.as_secs().max(1)
                ),
            }))
            .await?;
        return Ok(None);
    }

    let bootstrap_hello = match read_frame_with_timeout(&mut socket, PAIRING_TIMEOUT).await {
        Ok(frame) => frame,
        Err(err) => {
            register_pairing_failure(pairing_throttle, &remote_peer_key).await;
            return Err(err);
        }
    };
    let bootstrap_hello = match bootstrap_hello {
        Frame::Control(ControlMessage::BootstrapHello {
            protocol_version,
            client_bootstrap_public_key,
        }) => (protocol_version, client_bootstrap_public_key),
        _ => {
            FrameWriter::new(&mut socket)
                .write_frame(Frame::Control(ControlMessage::Error {
                    message: "未信任设备必须先发送最小 bootstrap 请求。".to_string(),
                }))
                .await?;
            return Ok(None);
        }
    };
    let (protocol_version, client_bootstrap_public_key) = bootstrap_hello;
    if protocol_version != PROTOCOL_VERSION {
        FrameWriter::new(&mut socket)
            .write_frame(Frame::Control(ControlMessage::Error {
                message: format!("不支持的协议版本: {protocol_version}"),
            }))
            .await?;
        return Ok(None);
    }

    let client_display = crypto::bootstrap_public_key_display(&client_bootstrap_public_key)?;
    let request_id = Uuid::new_v4().to_string();
    let server_bootstrap_key = crypto::generate_bootstrap_key_material()?;
    let server_bootstrap_public_key = server_bootstrap_key.public_key_encoded();
    let session_display = crypto::bootstrap_session_display(
        &request_id,
        &client_bootstrap_public_key,
        &server_bootstrap_public_key,
    )?;
    let pin = options
        .pairing
        .pin
        .clone()
        .unwrap_or_else(crypto::random_pin);

    println!();
    println!("{}", style("收到未信任设备的最小配对请求").bold());
    println!("地址: {}", remote_label);
    println!("客户端 bootstrap 指纹: {}", client_display.short);
    println!("{}", client_display.randomart);
    println!("本次会话核对图: {}", session_display.short);
    println!("{}", session_display.randomart);
    if options.pairing.pin.is_some() {
        println!("固定 PIN: {}", style(&pin).bold());
        println!("请让对方先核对上面的图形，再输入这个固定 PIN。");
    } else {
        println!("本次 PIN: {}", style(&pin).bold());
        println!("这个 PIN 只绑定到上面这组 bootstrap / 会话指纹。");
    }

    let (pake_state, server_pake_message) = crypto::start_bootstrap_pake_server(
        &pin,
        &request_id,
        &client_bootstrap_public_key,
        &server_bootstrap_public_key,
    )?;

    FrameWriter::new(&mut socket)
        .write_frame(Frame::Control(ControlMessage::BootstrapChallenge {
            request_id: request_id.clone(),
            server_bootstrap_public_key: server_bootstrap_public_key.clone(),
            server_pake_message,
        }))
        .await?;

    let pake_frame = match read_frame_with_timeout(&mut socket, PAIRING_TIMEOUT).await {
        Ok(frame) => frame,
        Err(err) => {
            register_pairing_failure(pairing_throttle, &remote_peer_key).await;
            return Err(err);
        }
    };
    let (client_pake_message, client_confirm) = match pake_frame {
        Frame::Control(ControlMessage::BootstrapPake {
            request_id: incoming_request_id,
            client_pake_message,
            client_confirm,
        }) if incoming_request_id == request_id => (client_pake_message, client_confirm),
        Frame::Control(ControlMessage::BootstrapPake { .. }) => {
            register_pairing_failure(pairing_throttle, &remote_peer_key).await;
            FrameWriter::new(&mut socket)
                .write_frame(Frame::Control(ControlMessage::Error {
                    message: "收到的 PAKE 请求标识与当前连接不匹配。".to_string(),
                }))
                .await?;
            return Ok(None);
        }
        _ => {
            register_pairing_failure(pairing_throttle, &remote_peer_key).await;
            FrameWriter::new(&mut socket)
                .write_frame(Frame::Control(ControlMessage::Error {
                    message: "客户端没有按预期完成 PAKE 认证。".to_string(),
                }))
                .await?;
            return Ok(None);
        }
    };

    let pake_key =
        match crypto::finish_bootstrap_pake(pake_state, &client_pake_message).and_then(|pake_key| {
            crypto::verify_client_pake_confirm(
                &pake_key,
                &request_id,
                &client_bootstrap_public_key,
                &server_bootstrap_public_key,
                &client_confirm,
            )?;
            Ok(pake_key)
        }) {
            Ok(pake_key) => pake_key,
            Err(err) => {
                register_pairing_failure(pairing_throttle, &remote_peer_key).await;
                FrameWriter::new(&mut socket)
                    .write_frame(Frame::Control(ControlMessage::Error {
                        message: format!("PIN 或 PAKE 认证失败：{err:#}"),
                    }))
                    .await?;
                return Ok(None);
            }
        };

    pairing_throttle.note_success(&remote_peer_key);
    let server_confirm = crypto::server_pake_confirm(
        &pake_key,
        &request_id,
        &client_bootstrap_public_key,
        &server_bootstrap_public_key,
    );
    FrameWriter::new(&mut socket)
        .write_frame(Frame::Control(ControlMessage::BootstrapAck {
            request_id: request_id.clone(),
            server_confirm,
        }))
        .await?;

    let acceptor = crypto::build_bootstrap_server_acceptor(
        &request_id,
        &pake_key,
        server_bootstrap_key,
        &client_bootstrap_public_key,
    )?;
    let device = config.device.clone();
    let mut server_stream = time::timeout(TLS_UPGRADE_TIMEOUT, acceptor.accept(socket))
        .await
        .map_err(|_| anyhow!("等待客户端切换到临时 mTLS 超时"))??;
    let frame = read_frame_with_timeout(&mut server_stream, PAIRING_TIMEOUT).await?;
    let (incoming_request_id, payload, trusted_proof) = match frame {
        Frame::Control(ControlMessage::PairRequest {
            request_id,
            payload,
            trusted_proof,
        }) => (request_id, payload, trusted_proof),
        _ => {
            FrameWriter::new(&mut server_stream)
                .write_frame(Frame::Control(ControlMessage::Error {
                    message: "临时 mTLS 已建立，但请求格式不正确。".to_string(),
                }))
                .await?;
            return Ok(None);
        }
    };
    if incoming_request_id != request_id {
        FrameWriter::new(&mut server_stream)
            .write_frame(Frame::Control(ControlMessage::Error {
                message: "收到的请求标识与当前 bootstrap 会话不匹配。".to_string(),
            }))
            .await?;
        return Ok(None);
    }
    if trusted_proof.is_some() {
        FrameWriter::new(&mut server_stream)
            .write_frame(Frame::Control(ControlMessage::Error {
                message: "bootstrap 配对阶段不接受 trusted-device 签名。".to_string(),
            }))
            .await?;
        return Ok(None);
    }
    if payload.protocol_version != PROTOCOL_VERSION {
        FrameWriter::new(&mut server_stream)
            .write_frame(Frame::Control(ControlMessage::Error {
                message: format!("不支持的协议版本: {}", payload.protocol_version),
            }))
            .await?;
        return Ok(None);
    }
    if let Err(err) = crypto::verify_device_identity_material(&payload.client) {
        FrameWriter::new(&mut server_stream)
            .write_frame(Frame::Control(ControlMessage::Error {
                message: format!("对端提供的设备身份材料无效: {err:#}"),
            }))
            .await?;
        return Ok(None);
    }

    let exporter = crypto::export_keying_material_from_server(&server_stream, &request_id)?;
    let agreement = negotiate(options.mode, payload.requested_mode);
    print_pair_request_overview(&payload, options, &agreement, &remote_label)?;
    if !agreement.any_direction() {
        FrameWriter::new(&mut server_stream)
            .write_frame(Frame::Control(ControlMessage::Error {
                message: "双方模式不兼容，本次请求无法建立同步。".to_string(),
            }))
            .await?;
        return Ok(None);
    }

    println!("已建立基于 PIN 的临时 mTLS，设备元数据现在处于加密保护中。");
    let accepted = if options.pairing.accept {
        true
    } else {
        prompt_confirm("接受这次同步吗", true)?
    };
    let trust_established = accepted && options.pairing.trust_device && payload.request_trust;
    let message = if accepted {
        "服务端已接受同步请求。".to_string()
    } else {
        "服务端拒绝了本次同步请求。".to_string()
    };
    let control = signed_pair_decision(PairDecisionParams {
        exporter: &exporter,
        request_id: &request_id,
        accepted,
        message,
        device: &device,
        workspace: &options.workspace,
        sync_clipboard: options.sync_clipboard,
        agreement: &agreement,
        auth_method: PairAuthMethod::Pin,
        pin: Some(&pin),
        trust_established,
    })?;
    FrameWriter::new(&mut server_stream)
        .write_frame(Frame::Control(control))
        .await?;

    if trust_established {
        config.remember_trusted_device(
            payload.client.device_id,
            payload.client.device_name.clone(),
            payload.client.identity_public_key.clone(),
            payload.client.tls_root_certificate.clone(),
        );
        config.save()?;
        println!("已记住该设备的身份公钥和 TLS 根证书，后续连接会使用长期 mTLS 并可免 PIN。");
    }

    if !accepted {
        return Ok(None);
    }

    let tls_stream: TlsStream<TcpStream> = server_stream.into();
    Ok(Some(AuthenticatedSession {
        role: SessionRole::Host,
        stream: tls_stream,
        remote: payload.client,
        agreement,
        remote_workspace: payload.workspace,
    }))
}

async fn connect_to_trusted_peer(
    address: std::net::Ipv4Addr,
    port: u16,
    device: &DeviceConfig,
    trusted_device: &TrustedDeviceConfig,
    config: &mut SynlyConfig,
    options: &RuntimeOptions,
) -> Result<AuthenticatedSession> {
    let socket = TcpStream::connect((address, port))
        .await
        .with_context(|| format!("failed to connect to {}:{}", address, port))?;
    let connector =
        crypto::build_client_connector(device, trusted_device.tls_root_certificate.as_str())?;
    let mut client_stream = connector.connect(crypto::server_name()?, socket).await?;

    let request_id = Uuid::new_v4().to_string();
    let exporter = crypto::export_keying_material_from_client(&client_stream, &request_id)?;
    let payload = PairRequestPayload {
        protocol_version: PROTOCOL_VERSION,
        client: device_identity(device),
        requested_mode: options.mode,
        workspace: options.workspace.summary(options.sync_clipboard),
        request_trust: options.pairing.trust_device,
    };
    let trusted_proof = crypto::sign_trusted_pair_auth(
        &exporter,
        device.identity_private_key()?,
        &request_id,
        &payload,
    )?;
    FrameWriter::new(&mut client_stream)
        .write_frame(Frame::Control(ControlMessage::PairRequest {
            request_id: request_id.clone(),
            payload: payload.clone(),
            trusted_proof: Some(trusted_proof),
        }))
        .await?;

    let reply = match FrameReader::new(&mut client_stream).read_frame().await? {
        Frame::Control(message) => message,
        _ => bail!("peer sent a non-control response during trusted pairing"),
    };
    let (remote, remote_workspace, agreement) = match reply {
        ControlMessage::PairDecision {
            accepted,
            message,
            server,
            workspace,
            agreement,
            auth_method,
            proof,
            trust_established,
        } => {
            if auth_method != PairAuthMethod::TrustedDevice {
                bail!("peer replied to trusted mTLS with an unexpected auth method");
            }
            let decision = ControlMessage::PairDecision {
                accepted,
                message: message.clone(),
                server: server.clone(),
                workspace: workspace.clone(),
                agreement: agreement.clone(),
                auth_method,
                proof,
                trust_established,
            };
            crypto::verify_device_identity_material(&server)?;
            crypto::verify_device_identity(&server, &trusted_device.public_key)?;
            crypto::verify_trusted_pair_decision(
                &decision,
                &exporter,
                &request_id,
                &trusted_device.public_key,
            )?;
            if !accepted {
                bail!("{}", message);
            }
            (server, workspace, agreement)
        }
        ControlMessage::Error { message } => bail!("{}", message),
        other => bail!("unexpected trusted pairing response: {other:?}"),
    };

    config.note_trusted_device_session(remote.device_id, &remote.device_name);
    config.save()?;

    let tls_stream: TlsStream<TcpStream> = client_stream.into();
    print_connected_peer(&remote, &agreement)?;
    Ok(AuthenticatedSession {
        role: SessionRole::Client,
        stream: tls_stream,
        remote,
        agreement,
        remote_workspace,
    })
}

async fn connect_to_untrusted_peer(
    address: std::net::Ipv4Addr,
    port: u16,
    device: &DeviceConfig,
    config: &mut SynlyConfig,
    options: &RuntimeOptions,
) -> Result<AuthenticatedSession> {
    let mut socket = TcpStream::connect((address, port))
        .await
        .with_context(|| format!("failed to connect to {}:{}", address, port))?;
    let client_bootstrap_key = crypto::generate_bootstrap_key_material()?;
    let client_bootstrap_public_key = client_bootstrap_key.public_key_encoded();
    let client_display = crypto::bootstrap_public_key_display(&client_bootstrap_public_key)?;

    println!();
    println!("{}", style("发起最小配对请求").bold());
    println!("本机 bootstrap 指纹: {}", client_display.short);
    println!("{}", client_display.randomart);
    println!("请确认 host 屏幕上显示的是同一张 bootstrap 图，再继续输入 PIN。");

    FrameWriter::new(&mut socket)
        .write_frame(Frame::Control(ControlMessage::BootstrapHello {
            protocol_version: PROTOCOL_VERSION,
            client_bootstrap_public_key: client_bootstrap_public_key.clone(),
        }))
        .await?;

    let (request_id, server_bootstrap_public_key, server_pake_message) =
        match read_frame_with_timeout(&mut socket, PAIRING_TIMEOUT).await? {
            Frame::Control(ControlMessage::BootstrapChallenge {
                request_id,
                server_bootstrap_public_key,
                server_pake_message,
            }) => (request_id, server_bootstrap_public_key, server_pake_message),
            Frame::Control(ControlMessage::Error { message }) => bail!("{}", message),
            other => bail!("unexpected bootstrap response: {other:?}"),
        };
    let session_display = crypto::bootstrap_session_display(
        &request_id,
        &client_bootstrap_public_key,
        &server_bootstrap_public_key,
    )?;

    println!("本次会话核对图: {}", session_display.short);
    println!("{}", session_display.randomart);
    let pin = resolve_pairing_pin(
        options.pairing.pin.as_deref(),
        "先核对 host 屏幕上的 bootstrap 图和会话图都与本机一致，再输入对应的 6 位 PIN",
    )?;
    let (pake_state, client_pake_message) = crypto::start_bootstrap_pake_client(
        &pin,
        &request_id,
        &client_bootstrap_public_key,
        &server_bootstrap_public_key,
    )?;
    let pake_key = crypto::finish_bootstrap_pake(pake_state, &server_pake_message)?;
    let client_confirm = crypto::client_pake_confirm(
        &pake_key,
        &request_id,
        &client_bootstrap_public_key,
        &server_bootstrap_public_key,
    );

    FrameWriter::new(&mut socket)
        .write_frame(Frame::Control(ControlMessage::BootstrapPake {
            request_id: request_id.clone(),
            client_pake_message,
            client_confirm,
        }))
        .await?;

    match read_frame_with_timeout(&mut socket, PAIRING_TIMEOUT).await? {
        Frame::Control(ControlMessage::BootstrapAck {
            request_id: incoming_request_id,
            server_confirm,
        }) if incoming_request_id == request_id => {
            crypto::verify_server_pake_confirm(
                &pake_key,
                &request_id,
                &client_bootstrap_public_key,
                &server_bootstrap_public_key,
                &server_confirm,
            )?;
        }
        Frame::Control(ControlMessage::BootstrapAck { .. }) => {
            bail!("peer returned a mismatched bootstrap acknowledgment");
        }
        Frame::Control(ControlMessage::Error { message }) => bail!("{}", message),
        other => bail!("unexpected PAKE response: {other:?}"),
    }

    let connector = crypto::build_bootstrap_client_connector(
        &request_id,
        &pake_key,
        client_bootstrap_key,
        &server_bootstrap_public_key,
    )?;
    let mut client_stream = time::timeout(
        TLS_UPGRADE_TIMEOUT,
        connector.connect(crypto::server_name()?, socket),
    )
    .await
    .map_err(|_| anyhow!("等待服务端切换到临时 mTLS 超时"))??;
    let exporter = crypto::export_keying_material_from_client(&client_stream, &request_id)?;
    let payload = PairRequestPayload {
        protocol_version: PROTOCOL_VERSION,
        client: device_identity(device),
        requested_mode: options.mode,
        workspace: options.workspace.summary(options.sync_clipboard),
        request_trust: options.pairing.trust_device,
    };
    FrameWriter::new(&mut client_stream)
        .write_frame(Frame::Control(ControlMessage::PairRequest {
            request_id: request_id.clone(),
            payload: payload.clone(),
            trusted_proof: None,
        }))
        .await?;

    let reply = match read_frame_with_timeout(&mut client_stream, PAIRING_TIMEOUT).await? {
        Frame::Control(message) => message,
        _ => bail!("peer sent a non-control response during bootstrap pairing"),
    };
    let (remote, remote_workspace, agreement) = match &reply {
        ControlMessage::PairDecision {
            accepted,
            message,
            server,
            workspace,
            agreement,
            auth_method,
            ..
        } => {
            if *auth_method != PairAuthMethod::Pin {
                bail!("bootstrap pairing expected a PIN-bound decision");
            }
            crypto::verify_device_identity_material(server)?;
            crypto::verify_pair_decision(&reply, &exporter, &request_id, &pin)?;
            if !accepted {
                bail!("{}", message);
            }
            (server.clone(), workspace.clone(), agreement.clone())
        }
        ControlMessage::Error { message } => bail!("{}", message),
        other => bail!("unexpected bootstrap pairing response: {other:?}"),
    };

    if let ControlMessage::PairDecision {
        trust_established, ..
    } = &reply
        && *trust_established
        && options.pairing.trust_device
    {
        config.remember_trusted_device(
            remote.device_id,
            remote.device_name.clone(),
            remote.identity_public_key.clone(),
            remote.tls_root_certificate.clone(),
        );
        config.save()?;
        println!("已保存对端身份公钥和 TLS 根证书，后续连接会使用长期 mTLS 并可免 PIN。");
    }

    let tls_stream: TlsStream<TcpStream> = client_stream.into();
    print_connected_peer(&remote, &agreement)?;
    Ok(AuthenticatedSession {
        role: SessionRole::Client,
        stream: tls_stream,
        remote,
        agreement,
        remote_workspace,
    })
}

async fn run_sync_session(
    session: AuthenticatedSession,
    workspace: &WorkspaceSpec,
    interval_secs: u64,
    sync_delete: bool,
    sync_clipboard: bool,
    clipboard_options: &crate::cli::ClipboardRuntimeOptions,
) -> Result<()> {
    println!();
    println!("{}", style("同步已开始").bold());
    println!(
        "连接到: {} ({})",
        session.remote.device_name,
        short_uuid(&session.remote.device_id)
    );
    for line in session.remote_workspace.human_lines() {
        println!("对端 {}", line);
    }

    let local_can_send = allows_local_send(session.role, &session.agreement);
    let local_can_receive = allows_local_receive(session.role, &session.agreement);
    let file_can_send = local_can_send
        && workspace.can_send_files()
        && session.remote_workspace.can_receive_files();
    let file_can_receive = local_can_receive
        && workspace.can_receive_files()
        && session.remote_workspace.can_send_files();
    let clipboard_enabled_on_both = sync_clipboard && session.remote_workspace.sync_clipboard;
    let clipboard_can_send = clipboard_enabled_on_both && local_can_send;
    let clipboard_can_receive = clipboard_enabled_on_both && local_can_receive;

    match (sync_clipboard, session.remote_workspace.sync_clipboard) {
        (true, true) => println!(
            "本次剪贴板同步: {}",
            clipboard_agreement_label(session.role, &session.agreement)
        ),
        (true, false) => println!("本次剪贴板同步: 本机已开启，但对端未开启，本次不会同步。"),
        (false, true) => println!("本次剪贴板同步: 对端已开启，但本机未开启，本次不会同步。"),
        (false, false) => println!("本次剪贴板同步: 关闭"),
    }

    let (read_half, write_half) = tokio::io::split(session.stream);
    let (tx, rx) = mpsc::channel::<Frame>(64);
    let writer_task = tokio::spawn(writer_loop(write_half, rx));

    let snapshot_task = if file_can_send {
        let outgoing = workspace
            .outgoing
            .clone()
            .context("session negotiated sending, but local workspace has no outgoing selection")?;
        let sender = tx.clone();
        Some(tokio::spawn(snapshot_loop(
            outgoing,
            sender,
            interval_secs.max(1),
        )))
    } else {
        None
    };
    let clipboard_sync = clipboard_enabled_on_both.then(|| {
        ClipboardSync::new(
            clipboard_options.max_file_bytes,
            clipboard_options.cache_dir.clone(),
        )
    });
    let (clipboard_watch_handle, clipboard_task) = if clipboard_can_send {
        let clipboard_sync = clipboard_sync
            .as_ref()
            .context("clipboard sync unexpectedly unavailable")?;
        let (clipboard_tx, clipboard_rx) = mpsc::unbounded_channel();
        let watcher = match clipboard_sync.start_local_watcher(clipboard_tx.clone()) {
            Ok(watcher) => Some(watcher),
            Err(err) => {
                eprintln!("无法启动剪贴板监听，本次将只接收远端剪贴板更新: {err:#}");
                None
            }
        };
        if watcher.is_some()
            && let Err(err) = clipboard_sync.publish_initial_payload(&clipboard_tx).await
        {
            eprintln!("无法读取当前剪贴板内容，已跳过初始剪贴板同步: {err:#}");
        }
        let sender = tx.clone();
        let task = Some(tokio::spawn(clipboard_sender_loop(clipboard_rx, sender)));
        (watcher, task)
    } else {
        (None, None)
    };

    let incoming_root = workspace.incoming_root.clone();
    let outgoing_spec = workspace.outgoing.clone();
    let mut reader = FrameReader::new(read_half);
    let mut pending_revisions = BTreeMap::<u64, PendingRevision>::new();
    let mut incoming_files = HashMap::<(u64, String), IncomingFileState>::new();
    loop {
        let frame = match reader.read_frame().await {
            Ok(frame) => frame,
            Err(err) => {
                if let Some(io_err) = err.downcast_ref::<std::io::Error>()
                    && io_err.kind() == std::io::ErrorKind::UnexpectedEof
                {
                    break;
                }
                return Err(err);
            }
        };

        match frame {
            Frame::Control(ControlMessage::SnapshotAdvert { revision, snapshot }) => {
                if !file_can_receive {
                    continue;
                }
                discard_superseded_revisions(&mut pending_revisions, &mut incoming_files, revision)
                    .await?;
                let root = incoming_root.as_ref().context(
                    "session negotiated receiving, but local workspace has no destination",
                )?;
                let snapshot = filter_snapshot_for_incoming_root(root, &snapshot)?;
                let local_snapshot = build_incoming_snapshot(root)?;
                let skipped_delete_count = if !sync_delete {
                    let preview_policy = delete_policy(snapshot.layout, true);
                    build_apply_plan(&snapshot, &local_snapshot, preview_policy)
                        .delete_paths
                        .len()
                } else {
                    0
                };
                let delete_policy = delete_policy(snapshot.layout, sync_delete);
                let plan = build_apply_plan(&snapshot, &local_snapshot, delete_policy);
                ensure_directories(root, &snapshot)?;

                if skipped_delete_count > 0 {
                    println!(
                        "检测到对端删除 {} 项；本机未开启删除同步，已保留本地文件。",
                        skipped_delete_count
                    );
                }

                if plan.file_requests.is_empty() {
                    let delete_report = delete_paths_best_effort(root, &plan.delete_paths);
                    print_delete_failures(&delete_report);
                    print_standalone_delete_result(&delete_report);
                } else {
                    pending_revisions.insert(
                        revision,
                        PendingRevision {
                            requested_files: plan.file_requests.len(),
                            remaining_files: plan.file_requests.iter().cloned().collect(),
                            failed_files: BTreeSet::new(),
                            delete_paths: plan.delete_paths,
                            transfer_done: false,
                        },
                    );
                    tx.send(Frame::Control(ControlMessage::FileRequest {
                        revision,
                        paths: plan.file_requests,
                    }))
                    .await?;
                }
            }
            Frame::Control(ControlMessage::FileRequest { revision, paths }) => {
                if !file_can_send {
                    continue;
                }
                let sender = tx.clone();
                let outgoing = outgoing_spec
                    .clone()
                    .context("no outgoing spec available for file request")?;
                tokio::spawn(async move {
                    if let Err(err) =
                        send_requested_files(sender.clone(), outgoing, revision, paths).await
                    {
                        let message = format!("发送修订版 {revision} 失败: {err:#}");
                        eprintln!("{message}");
                        let _ = sender
                            .send(Frame::Control(ControlMessage::TransferAborted {
                                revision,
                                message,
                            }))
                            .await;
                    }
                });
            }
            Frame::Control(ControlMessage::TransferDone { revision }) => {
                if let Some(pending) = pending_revisions.get_mut(&revision) {
                    pending.transfer_done = true;
                }
                maybe_finalize_revision(&incoming_root, &mut pending_revisions, revision);
            }
            Frame::Control(ControlMessage::TransferAborted { revision, message }) => {
                eprintln!("对端中止了修订版 {revision} 的传输: {message}");
                abort_revision(&mut pending_revisions, &mut incoming_files, revision).await?;
            }
            Frame::Control(ControlMessage::Error { message }) => {
                eprintln!("对端报告错误: {}", message);
            }
            Frame::Control(ControlMessage::Goodbye) => break,
            Frame::Control(ControlMessage::BootstrapHello { .. })
            | Frame::Control(ControlMessage::BootstrapChallenge { .. })
            | Frame::Control(ControlMessage::BootstrapPake { .. })
            | Frame::Control(ControlMessage::BootstrapAck { .. })
            | Frame::Control(ControlMessage::PairRequest { .. })
            | Frame::Control(ControlMessage::PinChallenge { .. })
            | Frame::Control(ControlMessage::PairAuth { .. })
            | Frame::Control(ControlMessage::PairDecision { .. }) => {
                bail!("received an unexpected pairing message after session start")
            }
            Frame::Clipboard(payload) => {
                if !clipboard_can_receive {
                    continue;
                }
                if let Some(clipboard_sync) = &clipboard_sync
                    && let Err(err) = clipboard_sync.apply_remote_payload(payload).await
                {
                    eprintln!("无法应用远端剪贴板内容: {err:#}");
                }
            }
            Frame::FileChunk(header, data) => {
                if !file_can_receive {
                    continue;
                }
                if !pending_revisions.contains_key(&header.revision) {
                    continue;
                }
                let root = incoming_root
                    .as_ref()
                    .context("received file data without a local destination")?;
                handle_file_chunk(
                    root,
                    &mut incoming_files,
                    &mut pending_revisions,
                    header,
                    data,
                )
                .await?;
            }
        }
    }

    drop(tx);
    if let Some(task) = snapshot_task {
        task.abort();
    }
    if let Some(watcher) = clipboard_watch_handle {
        watcher.stop();
    }
    if let Some(task) = clipboard_task {
        task.abort();
    }
    writer_task.await??;
    Ok(())
}

async fn writer_loop<W>(writer: W, mut rx: mpsc::Receiver<Frame>) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut writer = FrameWriter::new(writer);
    while let Some(frame) = rx.recv().await {
        writer.write_frame(frame).await?;
    }
    Ok(())
}

async fn clipboard_sender_loop(
    mut rx: mpsc::UnboundedReceiver<ClipboardPayload>,
    tx: mpsc::Sender<Frame>,
) -> Result<()> {
    while let Some(payload) = rx.recv().await {
        tx.send(Frame::Clipboard(payload)).await?;
    }
    Ok(())
}

async fn snapshot_loop(
    outgoing: crate::sync::OutgoingSpec,
    tx: mpsc::Sender<Frame>,
    interval_secs: u64,
) -> Result<()> {
    let (watch_tx, mut watch_rx) = mpsc::unbounded_channel::<notify::Result<Event>>();
    let mut watcher = RecommendedWatcher::new(
        move |event| {
            let _ = watch_tx.send(event);
        },
        NotifyConfig::default(),
    )
    .context("failed to start filesystem watcher")?;

    for target in watch_targets(&outgoing)? {
        let mode = if target.recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        watcher
            .watch(&target.path, mode)
            .with_context(|| format!("failed to watch shared path {}", target.path.display()))?;
    }

    let mut ticker = time::interval(Duration::from_secs(interval_secs.max(1)));
    let mut last_snapshot = None;
    let mut revision = 1u64;
    let debounce = Duration::from_millis(300);

    publish_snapshot_if_changed(&outgoing, &tx, &mut last_snapshot, &mut revision).await?;
    ticker.tick().await;

    loop {
        tokio::select! {
            maybe_event = watch_rx.recv() => {
                let event = match maybe_event {
                    Some(event) => event,
                    None => bail!("filesystem watcher channel closed unexpectedly"),
                };

                if let Err(err) = event {
                    eprintln!("文件监视出错，将等待下一次重扫: {err}");
                    continue;
                }

                drain_watch_events(&mut watch_rx, debounce).await;
                publish_snapshot_if_changed(&outgoing, &tx, &mut last_snapshot, &mut revision).await?;
            }
            _ = ticker.tick() => {
                publish_snapshot_if_changed(&outgoing, &tx, &mut last_snapshot, &mut revision).await?;
            }
        }
    }
}

async fn publish_snapshot_if_changed(
    outgoing: &crate::sync::OutgoingSpec,
    tx: &mpsc::Sender<Frame>,
    last_snapshot: &mut Option<crate::sync::ManifestSnapshot>,
    revision: &mut u64,
) -> Result<()> {
    let snapshot = build_snapshot(outgoing)?;
    if last_snapshot.as_ref() == Some(&snapshot) {
        return Ok(());
    }

    tx.send(Frame::Control(ControlMessage::SnapshotAdvert {
        revision: *revision,
        snapshot: snapshot.clone(),
    }))
    .await?;
    *last_snapshot = Some(snapshot);
    *revision += 1;
    Ok(())
}

async fn drain_watch_events(
    watch_rx: &mut mpsc::UnboundedReceiver<notify::Result<Event>>,
    debounce: Duration,
) {
    let sleep = time::sleep(debounce);
    tokio::pin!(sleep);

    loop {
        tokio::select! {
            _ = &mut sleep => break,
            maybe_event = watch_rx.recv() => match maybe_event {
                Some(Ok(_)) => {
                    sleep.as_mut().reset(Instant::now() + debounce);
                }
                Some(Err(err)) => {
                    eprintln!("文件监视出错，将继续等待变更稳定: {err}");
                    sleep.as_mut().reset(Instant::now() + debounce);
                }
                None => break,
            }
        }
    }
}

async fn send_requested_files(
    tx: mpsc::Sender<Frame>,
    outgoing: crate::sync::OutgoingSpec,
    revision: u64,
    paths: Vec<String>,
) -> Result<()> {
    let snapshot = build_snapshot(&outgoing)?;
    for path in paths {
        if !snapshot_contains_file(&snapshot, &path)? {
            bail!("requested path `{path}` is not part of the advertised snapshot");
        }
        send_one_file(&tx, &outgoing, revision, &path).await?;
    }

    tx.send(Frame::Control(ControlMessage::TransferDone { revision }))
        .await?;
    Ok(())
}

async fn send_one_file(
    tx: &mpsc::Sender<Frame>,
    outgoing: &crate::sync::OutgoingSpec,
    revision: u64,
    wire_path: &str,
) -> Result<()> {
    let path = resolve_outgoing_path(outgoing, wire_path)?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if !metadata.is_file() {
        bail!("requested path {} is not a regular file", path.display());
    }

    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    let executable = is_executable(&metadata);

    let mut file = File::open(&path)
        .await
        .with_context(|| format!("failed to open {}", path.display()))?;
    let mut offset = 0u64;
    let total_size = metadata.len();
    let mut buffer = vec![0u8; FILE_CHUNK_SIZE];

    if total_size == 0 {
        tx.send(Frame::FileChunk(
            FileChunkHeader {
                revision,
                path: wire_path.to_string(),
                offset: 0,
                total_size: 0,
                modified_ms,
                executable,
                final_chunk: true,
            },
            Vec::new(),
        ))
        .await?;
        return Ok(());
    }

    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let final_chunk = offset + read as u64 >= total_size;
        tx.send(Frame::FileChunk(
            FileChunkHeader {
                revision,
                path: wire_path.to_string(),
                offset,
                total_size,
                modified_ms,
                executable,
                final_chunk,
            },
            buffer[..read].to_vec(),
        ))
        .await?;
        offset += read as u64;
        if final_chunk {
            break;
        }
    }

    Ok(())
}

async fn handle_file_chunk(
    root: &Path,
    incoming_files: &mut HashMap<(u64, String), IncomingFileState>,
    pending_revisions: &mut BTreeMap<u64, PendingRevision>,
    header: FileChunkHeader,
    data: Vec<u8>,
) -> Result<()> {
    if pending_revisions
        .get(&header.revision)
        .is_some_and(|pending| pending.failed_files.contains(&header.path))
    {
        return Ok(());
    }

    let key = (header.revision, header.path.clone());
    if header.offset == 0 {
        match begin_incoming_file(root, &header).await {
            Ok(state) => {
                incoming_files.insert(key.clone(), state);
            }
            Err((final_path, err)) => {
                report_incoming_file_failure(
                    root,
                    incoming_files,
                    pending_revisions,
                    header.revision,
                    &header.path,
                    final_path,
                    None,
                    err,
                )
                .await;
                return Ok(());
            }
        }
    }

    let write_result = {
        let state = match incoming_files.get_mut(&key) {
            Some(state) => state,
            None => {
                report_incoming_file_failure(
                    root,
                    incoming_files,
                    pending_revisions,
                    header.revision,
                    &header.path,
                    None,
                    None,
                    anyhow!("missing transfer state for {}", header.path),
                )
                .await;
                return Ok(());
            }
        };

        if state.written != header.offset {
            Err((
                Some(state.final_path.clone()),
                Some(state.temp_path.clone()),
                anyhow!(
                    "unexpected file chunk offset for {}: expected {}, got {}",
                    header.path,
                    state.written,
                    header.offset
                ),
            ))
        } else if let Err(err) = state.file.write_all(&data).await {
            Err((
                Some(state.final_path.clone()),
                Some(state.temp_path.clone()),
                err.into(),
            ))
        } else {
            state.written += data.len() as u64;
            Ok(())
        }
    };

    if let Err((final_path, temp_path, err)) = write_result {
        report_incoming_file_failure(
            root,
            incoming_files,
            pending_revisions,
            header.revision,
            &header.path,
            final_path,
            temp_path,
            err,
        )
        .await;
        return Ok(());
    }

    if header.final_chunk {
        let state = match incoming_files.remove(&key) {
            Some(state) => state,
            None => {
                report_incoming_file_failure(
                    root,
                    incoming_files,
                    pending_revisions,
                    header.revision,
                    &header.path,
                    None,
                    None,
                    anyhow!("missing final transfer state for {}", header.path),
                )
                .await;
                return Ok(());
            }
        };
        let final_path = Some(state.final_path.clone());
        let temp_path = Some(state.temp_path.clone());

        if let Err(err) = finalize_incoming_file(state).await {
            report_incoming_file_failure(
                root,
                incoming_files,
                pending_revisions,
                header.revision,
                &header.path,
                final_path,
                temp_path,
                err,
            )
            .await;
            return Ok(());
        }

        if let Some(pending) = pending_revisions.get_mut(&header.revision) {
            pending.remaining_files.remove(&header.path);
        }
        maybe_finalize_revision(
            &Some(root.to_path_buf()),
            pending_revisions,
            header.revision,
        );
    }

    Ok(())
}

fn maybe_finalize_revision(
    incoming_root: &Option<PathBuf>,
    pending_revisions: &mut BTreeMap<u64, PendingRevision>,
    revision: u64,
) {
    let should_finalize = pending_revisions
        .get(&revision)
        .is_some_and(|pending| pending.transfer_done && pending.remaining_files.is_empty());

    if should_finalize
        && let Some(pending) = pending_revisions.remove(&revision)
        && let Some(root) = incoming_root
    {
        let delete_report = delete_paths_best_effort(root, &pending.delete_paths);
        print_delete_failures(&delete_report);
        let updated_files = pending
            .requested_files
            .saturating_sub(pending.failed_files.len());
        print_revision_result(updated_files, pending.failed_files.len(), &delete_report);
    }
}

async fn discard_superseded_revisions(
    pending_revisions: &mut BTreeMap<u64, PendingRevision>,
    incoming_files: &mut HashMap<(u64, String), IncomingFileState>,
    keep_revision: u64,
) -> Result<()> {
    let stale_revisions = pending_revisions
        .keys()
        .copied()
        .filter(|revision| *revision < keep_revision)
        .collect::<Vec<_>>();

    for revision in stale_revisions {
        abort_revision(pending_revisions, incoming_files, revision).await?;
    }

    Ok(())
}

async fn abort_revision(
    pending_revisions: &mut BTreeMap<u64, PendingRevision>,
    incoming_files: &mut HashMap<(u64, String), IncomingFileState>,
    revision: u64,
) -> Result<()> {
    pending_revisions.remove(&revision);

    let stale_files = incoming_files
        .keys()
        .filter(|(file_revision, _)| *file_revision == revision)
        .cloned()
        .collect::<Vec<_>>();

    for key in stale_files {
        if let Some(state) = incoming_files.remove(&key) {
            let _ = tokio::fs::remove_file(&state.temp_path).await;
        }
    }

    Ok(())
}

async fn replace_destination(destination: &Path, temp_path: &Path) -> Result<()> {
    if let Ok(metadata) = tokio::fs::symlink_metadata(destination).await {
        if metadata.file_type().is_symlink() || metadata.is_file() {
            tokio::fs::remove_file(destination).await?;
        } else if metadata.is_dir() {
            tokio::fs::remove_dir_all(destination).await?;
        }
    }
    tokio::fs::rename(temp_path, destination).await?;
    Ok(())
}

async fn begin_incoming_file(
    root: &Path,
    header: &FileChunkHeader,
) -> std::result::Result<IncomingFileState, (Option<PathBuf>, anyhow::Error)> {
    let final_path = match resolve_incoming_path(root, &header.path) {
        Ok(path) => path,
        Err(err) => return Err((None, err)),
    };

    if let Some(parent) = final_path.parent()
        && let Err(err) = tokio::fs::create_dir_all(parent).await
    {
        return Err((Some(final_path), err.into()));
    }

    let temp_path = temp_file_path(&final_path);
    let _ = tokio::fs::remove_file(&temp_path).await;
    let file = match File::create(&temp_path).await {
        Ok(file) => file,
        Err(err) => return Err((Some(final_path), err.into())),
    };

    Ok(IncomingFileState {
        file,
        temp_path,
        final_path,
        modified_ms: header.modified_ms,
        executable: header.executable,
        expected_size: header.total_size,
        written: 0,
    })
}

async fn finalize_incoming_file(state: IncomingFileState) -> Result<()> {
    let IncomingFileState {
        mut file,
        temp_path,
        final_path,
        modified_ms,
        executable,
        expected_size,
        written,
    } = state;

    file.flush().await?;
    drop(file);

    if written != expected_size {
        bail!(
            "received size mismatch for {}: expected {}, got {}",
            final_path.display(),
            expected_size,
            written
        );
    }

    replace_destination(&final_path, &temp_path).await?;
    apply_file_metadata(&final_path, modified_ms, executable)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn report_incoming_file_failure(
    root: &Path,
    incoming_files: &mut HashMap<(u64, String), IncomingFileState>,
    pending_revisions: &mut BTreeMap<u64, PendingRevision>,
    revision: u64,
    wire_path: &str,
    final_path: Option<PathBuf>,
    temp_path: Option<PathBuf>,
    err: anyhow::Error,
) {
    let key = (revision, wire_path.to_string());
    let mut final_path = final_path;
    let mut temp_path = temp_path;

    if let Some(state) = incoming_files.remove(&key) {
        if final_path.is_none() {
            final_path = Some(state.final_path);
        }
        if temp_path.is_none() {
            temp_path = Some(state.temp_path);
        }
    }

    if let Some(temp_path) = temp_path {
        let _ = tokio::fs::remove_file(temp_path).await;
    }

    if let Some(pending) = pending_revisions.get_mut(&revision) {
        pending.remaining_files.remove(wire_path);
        pending.failed_files.insert(wire_path.to_string());
    }

    let target = final_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| wire_path.to_string());
    eprintln!("无法更新文件 {}: {err:#}", target);

    maybe_finalize_revision(&Some(root.to_path_buf()), pending_revisions, revision);
}

fn print_delete_failures(report: &crate::sync::DeleteReport) {
    for failure in &report.failures {
        let target = failure
            .local_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| failure.wire_path.clone());
        eprintln!("无法归档删除项 {}: {}", target, failure.reason);
    }
}

fn print_standalone_delete_result(report: &crate::sync::DeleteReport) {
    match (report.archived_count, report.failures.len()) {
        (0, 0) => println!("本地已是最新状态。"),
        (archived, 0) => {
            println!("已归档对端删除项，共 {} 项，位置: .synly/deleted", archived);
        }
        (0, failed) => {
            println!("有 {} 项对端删除未能归档，已保留本地文件。", failed);
        }
        (archived, failed) => {
            println!(
                "已归档对端删除项 {} 项，另有 {} 项未能归档，已保留本地文件。",
                archived, failed
            );
        }
    }
}

fn print_revision_result(
    updated_files: usize,
    failed_updates: usize,
    delete_report: &crate::sync::DeleteReport,
) {
    if failed_updates == 0 && delete_report.failures.is_empty() {
        println!(
            "已完成一次同步，更新 {} 个文件，归档删除 {} 项。",
            updated_files, delete_report.archived_count
        );
        return;
    }

    println!(
        "已完成一次同步，更新 {} 个文件，文件更新失败 {} 个，归档删除 {} 项，删除失败 {} 项。",
        updated_files,
        failed_updates,
        delete_report.archived_count,
        delete_report.failures.len()
    );
}

fn choose_peer(peer_query: Option<&str>, timeout: Duration) -> Result<DiscoveredPeer> {
    if let Some(query) = peer_query {
        let peers = discovery::browse(timeout)?;
        return select_peer_from_query(&peers, require_peer_query(Some(query))?);
    }

    loop {
        let peers = discovery::browse(timeout)?;
        if peers.is_empty() {
            if !prompt_confirm("继续搜索设备吗", true)? {
                bail!("no peer selected");
            }
            continue;
        }

        let options = peers.iter().map(DiscoveredPeer::label).collect::<Vec<_>>();
        let index = prompt_select("选择设备", &options, None)?;
        return Ok(peers[index].clone());
    }
}

fn select_peer_from_query(peers: &[DiscoveredPeer], query: &str) -> Result<DiscoveredPeer> {
    let matches = peers
        .iter()
        .filter(|peer| peer_matches_query(peer, query))
        .cloned()
        .collect::<Vec<_>>();

    match matches.len() {
        0 => bail!("没有找到匹配 `{query}` 的设备"),
        1 => Ok(matches[0].clone()),
        _ => {
            let labels = matches
                .iter()
                .map(DiscoveredPeer::label)
                .collect::<Vec<_>>()
                .join(" | ");
            bail!(
                "`{query}` 匹配到多个设备，请改用更精确的名称、设备 ID 前缀或 IPv4 地址: {labels}"
            )
        }
    }
}

fn peer_matches_query(peer: &DiscoveredPeer, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return false;
    }

    peer.device_name.eq_ignore_ascii_case(query)
        || peer.device_id.eq_ignore_ascii_case(query)
        || peer
            .device_id
            .to_ascii_lowercase()
            .starts_with(&query.to_ascii_lowercase())
        || peer
            .addresses
            .iter()
            .any(|address| address.to_string() == query)
}

fn trusted_device_for_peer(
    config: &SynlyConfig,
    peer: &DiscoveredPeer,
) -> Result<Option<TrustedDeviceConfig>> {
    let device_id = Uuid::parse_str(&peer.device_id)
        .with_context(|| format!("peer advertised an invalid device id: {}", peer.device_id))?;
    Ok(config.trusted_device(&device_id).cloned())
}

async fn read_frame_with_timeout<R>(reader: &mut R, timeout: Duration) -> Result<Frame>
where
    R: AsyncRead + Unpin,
{
    time::timeout(timeout, FrameReader::new(reader).read_frame())
        .await
        .map_err(|_| anyhow!("等待对端响应超时"))?
}

async fn register_pairing_failure(pairing_throttle: &mut PairingThrottle, peer_key: &str) {
    let backoff = pairing_throttle.note_failure(peer_key);
    if !backoff.is_zero() {
        time::sleep(backoff).await;
    }
}

impl PairingThrottle {
    fn blocked_remaining(&mut self, peer_key: &str) -> Option<Duration> {
        let now = Instant::now();
        let state = self.peers.get(peer_key)?;
        if now.duration_since(state.window_started_at) > PAIRING_FAILURE_WINDOW {
            self.peers.remove(peer_key);
            return None;
        }
        match state.blocked_until {
            Some(blocked_until) if blocked_until > now => Some(blocked_until.duration_since(now)),
            _ => None,
        }
    }

    fn note_failure(&mut self, peer_key: &str) -> Duration {
        let now = Instant::now();
        let state = self
            .peers
            .entry(peer_key.to_string())
            .or_insert(PairingPeerState {
                failures: 0,
                window_started_at: now,
                blocked_until: None,
            });
        if now.duration_since(state.window_started_at) > PAIRING_FAILURE_WINDOW {
            state.failures = 0;
            state.window_started_at = now;
            state.blocked_until = None;
        }
        state.failures = state.failures.saturating_add(1);
        if state.failures >= PAIRING_MAX_FAILURES {
            state.blocked_until = Some(now + PAIRING_COOLDOWN);
        }
        Duration::from_millis(
            PAIRING_BACKOFF_BASE_MS.saturating_mul(u64::from(state.failures.min(4))),
        )
    }

    fn note_success(&mut self, peer_key: &str) {
        self.peers.remove(peer_key);
    }
}

fn has_trusted_transport(config: &SynlyConfig) -> bool {
    config.trusted_devices.iter().any(|device| {
        !device.public_key.trim().is_empty() && !device.tls_root_certificate.trim().is_empty()
    })
}

fn print_pair_request_overview(
    payload: &PairRequestPayload,
    options: &RuntimeOptions,
    agreement: &SessionAgreement,
    remote_addr: &str,
) -> Result<()> {
    println!();
    println!("{}", style("收到同步请求").bold());
    println!(
        "来自: {} ({})",
        payload.client.device_name,
        short_uuid(&payload.client.device_id)
    );
    println!(
        "对端身份指纹: {}",
        crypto::short_identity_fingerprint(&payload.client.identity_public_key)?
    );
    println!("地址: {}", remote_addr);
    for line in payload.workspace.human_lines() {
        println!("{line}");
    }
    for line in options.workspace.local_human_lines(options.sync_clipboard) {
        println!("本机 {line}");
    }
    if options.workspace.incoming_root.is_some() {
        println!("本机 删除同步: {}", sync_delete_label(options.sync_delete));
    }
    println!(
        "协商结果: {}",
        agreement_label(SessionRole::Host, agreement)
    );
    Ok(())
}

fn print_connected_peer(remote: &DeviceIdentity, agreement: &SessionAgreement) -> Result<()> {
    println!();
    println!("{}", style("连接已建立").bold());
    println!(
        "对端: {} ({})",
        remote.device_name,
        short_uuid(&remote.device_id)
    );
    println!(
        "对端身份指纹: {}",
        crypto::short_identity_fingerprint(&remote.identity_public_key)?
    );
    println!(
        "协商结果: {}",
        agreement_label(SessionRole::Client, agreement)
    );
    Ok(())
}

fn negotiate(host_mode: SyncMode, client_mode: SyncMode) -> SessionAgreement {
    SessionAgreement {
        host_to_client: host_mode.can_send() && client_mode.can_receive(),
        client_to_host: client_mode.can_send() && host_mode.can_receive(),
    }
}

fn delete_policy(layout: crate::sync::SnapshotLayout, sync_delete: bool) -> DeletePolicy {
    if !sync_delete {
        return DeletePolicy::Never;
    }

    match layout {
        crate::sync::SnapshotLayout::RootContents => DeletePolicy::MirrorAll,
        crate::sync::SnapshotLayout::SelectedItems => DeletePolicy::MirrorSelectedItems,
    }
}

fn allows_local_send(role: SessionRole, agreement: &SessionAgreement) -> bool {
    match role {
        SessionRole::Host => agreement.host_to_client,
        SessionRole::Client => agreement.client_to_host,
    }
}

fn allows_local_receive(role: SessionRole, agreement: &SessionAgreement) -> bool {
    match role {
        SessionRole::Host => agreement.client_to_host,
        SessionRole::Client => agreement.host_to_client,
    }
}

fn signed_pair_decision(params: PairDecisionParams<'_>) -> Result<ControlMessage> {
    let summary = params.workspace.summary(params.sync_clipboard);
    let server = device_identity(params.device);
    let proof = match params.auth_method {
        PairAuthMethod::Pin => crypto::sign_pair_decision(
            params.exporter,
            params.request_id,
            params.pin.context("missing PIN for pair decision")?,
            params.accepted,
            &params.message,
            &server,
            params.agreement,
            &summary,
            params.auth_method,
            params.trust_established,
        )?,
        PairAuthMethod::TrustedDevice => crypto::sign_trusted_pair_decision(
            params.device.identity_private_key()?,
            params.exporter,
            params.request_id,
            params.accepted,
            &params.message,
            &server,
            params.agreement,
            &summary,
            params.trust_established,
        )?,
    };
    Ok(ControlMessage::PairDecision {
        accepted: params.accepted,
        message: params.message,
        server,
        workspace: summary,
        agreement: params.agreement.clone(),
        auth_method: params.auth_method,
        proof,
        trust_established: params.trust_established,
    })
}

fn device_identity(device: &DeviceConfig) -> DeviceIdentity {
    DeviceIdentity {
        device_id: device.device_id,
        device_name: device.device_name.clone(),
        identity_public_key: device
            .identity_public_key()
            .expect("device identity public key is missing")
            .to_string(),
        tls_root_certificate: crypto::device_tls_root_certificate(device)
            .expect("device TLS root certificate generation failed"),
    }
}

fn print_host_ready(device: &DeviceConfig, options: &RuntimeOptions, port: u16) {
    println!("{}", style("Synly 已就绪").bold());
    println!("设备: {} ({})", device.device_name, device.short_id());
    println!(
        "本机身份指纹: {}",
        crypto::short_identity_fingerprint(
            device
                .identity_public_key()
                .expect("device identity public key is missing"),
        )
        .expect("device identity fingerprint is invalid")
    );
    println!("模式: {}", options.mode.label());
    if !options.workspace.file_sync_enabled() {
        println!("文件同步: 关闭（仅剪贴板）");
    }
    println!(
        "剪贴板同步: {}",
        sync_clipboard_label(options.sync_clipboard)
    );
    if options.workspace.incoming_root.is_some() {
        println!("删除同步: {}", sync_delete_label(options.sync_delete));
    }
    println!(
        "配对策略: {}",
        if options.pairing.trusted_only {
            "仅可信设备"
        } else {
            "可信设备走长期 mTLS；未信任设备走 bootstrap + PIN + 临时 mTLS"
        }
    );
    println!(
        "接受策略: {}",
        if options.pairing.accept {
            "认证通过后自动接受"
        } else {
            "认证通过后仍需本机确认"
        }
    );
    if let Some(pin) = &options.pairing.pin {
        println!("固定 PIN: {}", style(pin).bold());
    }
    println!("监听端口: {}", port);
    println!("等待同步请求。");
    if options.pairing.trusted_only {
        println!("仅已建立可信设备公钥的设备可以连接。");
    } else if options.pairing.pin.is_some() {
        println!("未被信任的设备会先交换 bootstrap 指纹，再使用上面的固定 PIN 建立临时 mTLS。");
    } else {
        println!(
            "收到未被信任的请求后，会先显示 bootstrap 指纹和会话图，再为该请求单独显示 6 位 PIN。"
        );
    }
}

fn agreement_label(role: SessionRole, agreement: &SessionAgreement) -> &'static str {
    match (
        allows_local_send(role, agreement),
        allows_local_receive(role, agreement),
    ) {
        (true, true) => "双向同步",
        (true, false) => "本机 -> 对端",
        (false, true) => "对端 -> 本机",
        (false, false) => "无可用同步方向",
    }
}

fn clipboard_agreement_label(role: SessionRole, agreement: &SessionAgreement) -> &'static str {
    agreement_label(role, agreement)
}

fn temp_file_path(destination: &Path) -> PathBuf {
    let suffix = rand::rng().random_range(1000..9999);
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("synly");
    destination.with_file_name(format!(".{}.{}.synly.part", file_name, suffix))
}

fn short_uuid(id: &Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::{delete_policy, peer_matches_query, select_peer_from_query};
    use crate::cli::SyncMode;
    use crate::discovery::DiscoveredPeer;
    use crate::sync::{DeletePolicy, SnapshotLayout};
    use std::net::Ipv4Addr;

    #[test]
    fn delete_policy_stays_disabled_when_sync_delete_is_off() {
        assert!(matches!(
            delete_policy(SnapshotLayout::RootContents, false),
            DeletePolicy::Never
        ));
        assert!(matches!(
            delete_policy(SnapshotLayout::SelectedItems, false),
            DeletePolicy::Never
        ));
    }

    #[test]
    fn delete_policy_mirrors_root_contents_when_enabled() {
        assert!(matches!(
            delete_policy(SnapshotLayout::RootContents, true),
            DeletePolicy::MirrorAll
        ));
    }

    #[test]
    fn delete_policy_limits_selected_items_when_enabled() {
        assert!(matches!(
            delete_policy(SnapshotLayout::SelectedItems, true),
            DeletePolicy::MirrorSelectedItems
        ));
    }

    #[test]
    fn peer_query_matches_name_id_prefix_and_ip() {
        let peer = sample_peer();
        assert!(peer_matches_query(&peer, "demo-device"));
        assert!(peer_matches_query(&peer, "abcd1234"));
        assert!(peer_matches_query(&peer, "192.168.1.20"));
        assert!(!peer_matches_query(&peer, "unknown"));
    }

    #[test]
    fn select_peer_from_query_requires_unique_match() {
        let peer = sample_peer();
        let selected = select_peer_from_query(std::slice::from_ref(&peer), "demo-device").unwrap();
        assert_eq!(selected.device_id, peer.device_id);

        let duplicate = DiscoveredPeer {
            fullname: "dup".to_string(),
            device_name: "demo-device".to_string(),
            device_id: "ffffeeee-dddd-cccc-bbbb-aaaaaaaaaaaa".to_string(),
            mode: SyncMode::Auto,
            port: 9999,
            addresses: vec![Ipv4Addr::new(192, 168, 1, 21)],
        };
        assert!(select_peer_from_query(&[peer, duplicate], "demo-device").is_err());
    }

    fn sample_peer() -> DiscoveredPeer {
        DiscoveredPeer {
            fullname: "demo._synly._tcp.local.".to_string(),
            device_name: "demo-device".to_string(),
            device_id: "abcd1234-1111-2222-3333-444455556666".to_string(),
            mode: SyncMode::Both,
            port: 8080,
            addresses: vec![Ipv4Addr::new(192, 168, 1, 20)],
        }
    }
}
