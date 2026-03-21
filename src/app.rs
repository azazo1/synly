use crate::cli::{
    ConnectionPreference, RuntimeOptions, SyncMode, prompt_confirm, prompt_secret, prompt_select,
    sync_clipboard_label, sync_delete_label,
};
use crate::clipboard::ClipboardSync;
use crate::config::DeviceConfig;
use crate::crypto;
use crate::discovery::{self, Advertisement, DiscoveredPeer};
use crate::protocol::{
    ClipboardPayload, ControlMessage, DeviceIdentity, FileChunkHeader, Frame, FrameReader,
    FrameWriter, PROTOCOL_VERSION, PairRequestPayload, SessionAgreement,
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
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{self, Instant};
use tokio_rustls::TlsStream;
use uuid::Uuid;

const FILE_CHUNK_SIZE: usize = 256 * 1024;

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
    pin: &'a str,
    accepted: bool,
    message: String,
    device: &'a DeviceConfig,
    workspace: &'a WorkspaceSpec,
    sync_clipboard: bool,
    agreement: &'a SessionAgreement,
}

pub async fn run(device: DeviceConfig, options: RuntimeOptions) -> Result<()> {
    match options.connection {
        ConnectionPreference::Host => run_host(device, options).await,
        ConnectionPreference::Join => run_client(device, options).await,
    }
}

async fn run_host(device: DeviceConfig, options: RuntimeOptions) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", 0))
        .await
        .context("failed to bind TCP listener")?;
    let port = listener.local_addr()?.port();
    let acceptor = crypto::build_server_acceptor()?;
    let _advertisement = discovery::advertise(&Advertisement {
        port,
        device: device.clone(),
        mode: options.mode,
    })?;

    print_host_ready(&device, &options, port);

    loop {
        let (socket, address) = listener.accept().await?;
        match handle_incoming_connection(socket, address.to_string(), &acceptor, &device, &options)
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

async fn run_client(device: DeviceConfig, options: RuntimeOptions) -> Result<()> {
    loop {
        let peer = choose_peer()?;
        match connect_to_peer(&peer, &device, &options).await {
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
    remote_addr: String,
    acceptor: &tokio_rustls::TlsAcceptor,
    device: &DeviceConfig,
    options: &RuntimeOptions,
) -> Result<Option<AuthenticatedSession>> {
    let mut server_stream = acceptor.accept(socket).await?;
    let pair_result = {
        let frame = FrameReader::new(&mut server_stream).read_frame().await?;
        let payload = match frame {
            Frame::Control(ControlMessage::PairRequest { payload }) => payload,
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

        let agreement = negotiate(options.mode, payload.requested_mode);
        println!();
        println!("{}", style("收到同步请求").bold());
        println!(
            "来自: {} ({})",
            payload.client.device_name,
            short_uuid(&payload.client.device_id)
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
            agreement_label(SessionRole::Host, &agreement)
        );

        if !agreement.any_direction() {
            FrameWriter::new(&mut server_stream)
                .write_frame(Frame::Control(ControlMessage::Error {
                    message: "双方模式不兼容，本次请求无法建立同步。".to_string(),
                }))
                .await?;
            return Ok(None);
        }

        let request_id = Uuid::new_v4().to_string();
        let pin = crypto::random_pin();
        println!("本次 PIN: {}", style(&pin).bold());
        println!("请让对方输入这个 PIN。该 PIN 只适用于这一次请求。");

        FrameWriter::new(&mut server_stream)
            .write_frame(Frame::Control(ControlMessage::PinChallenge {
                request_id: request_id.clone(),
                server: device_identity(device),
                message: "请求信息已送达对端，请查看服务端屏幕上的本次 PIN。".to_string(),
            }))
            .await?;

        let exporter = crypto::export_keying_material_from_server(&server_stream, &request_id)?;
        let frame = FrameReader::new(&mut server_stream).read_frame().await?;
        let proof = match frame {
            Frame::Control(ControlMessage::PairAuth {
                request_id: incoming_request_id,
                proof,
            }) if incoming_request_id == request_id => proof,
            Frame::Control(ControlMessage::PairAuth { .. }) => {
                FrameWriter::new(&mut server_stream)
                    .write_frame(Frame::Control(ControlMessage::Error {
                        message: "收到的 PIN 请求标识与当前连接不匹配。".to_string(),
                    }))
                    .await?;
                return Ok(None);
            }
            _ => {
                FrameWriter::new(&mut server_stream)
                    .write_frame(Frame::Control(ControlMessage::Error {
                        message: "客户端没有按预期提交 PIN 校验信息。".to_string(),
                    }))
                    .await?;
                return Ok(None);
            }
        };

        if crypto::verify_pair_auth(&exporter, &request_id, &pin, &payload, &proof).is_err() {
            FrameWriter::new(&mut server_stream)
                .write_frame(Frame::Control(ControlMessage::Error {
                    message: "PIN 校验失败，本次连接未被接受。".to_string(),
                }))
                .await?;
            return Ok(None);
        }

        println!("PIN 校验通过，等待本机确认。");

        if !prompt_confirm("接受这次同步吗", true)? {
            let message = "服务端拒绝了本次同步请求。".to_string();
            let control = signed_pair_decision(PairDecisionParams {
                exporter: &exporter,
                request_id: &request_id,
                pin: &pin,
                accepted: false,
                message: message.clone(),
                device,
                workspace: &options.workspace,
                sync_clipboard: options.sync_clipboard,
                agreement: &agreement,
            })?;
            FrameWriter::new(&mut server_stream)
                .write_frame(Frame::Control(control))
                .await?;
            return Ok(None);
        }

        let message = "服务端已接受同步请求。".to_string();
        let control = signed_pair_decision(PairDecisionParams {
            exporter: &exporter,
            request_id: &request_id,
            pin: &pin,
            accepted: true,
            message,
            device,
            workspace: &options.workspace,
            sync_clipboard: options.sync_clipboard,
            agreement: &agreement,
        })?;
        FrameWriter::new(&mut server_stream)
            .write_frame(Frame::Control(control))
            .await?;

        Ok::<_, anyhow::Error>((payload.client, agreement, payload.workspace))
    }?;

    let (remote, agreement, remote_workspace) = pair_result;
    let tls_stream: TlsStream<TcpStream> = server_stream.into();
    Ok(Some(AuthenticatedSession {
        role: SessionRole::Host,
        stream: tls_stream,
        remote,
        agreement,
        remote_workspace,
    }))
}

async fn connect_to_peer(
    peer: &DiscoveredPeer,
    device: &DeviceConfig,
    options: &RuntimeOptions,
) -> Result<AuthenticatedSession> {
    let address = peer
        .addresses
        .first()
        .copied()
        .context("peer advertised no IPv4 address")?;
    let socket = TcpStream::connect((address, peer.port))
        .await
        .with_context(|| format!("failed to connect to {}:{}", address, peer.port))?;
    let connector = crypto::build_client_connector()?;
    let mut client_stream = connector.connect(crypto::server_name()?, socket).await?;

    let remote_info = {
        let payload = PairRequestPayload {
            protocol_version: PROTOCOL_VERSION,
            client: device_identity(device),
            requested_mode: options.mode,
            workspace: options.workspace.summary(options.sync_clipboard),
        };
        FrameWriter::new(&mut client_stream)
            .write_frame(Frame::Control(ControlMessage::PairRequest {
                payload: payload.clone(),
            }))
            .await?;

        let reply = match FrameReader::new(&mut client_stream).read_frame().await? {
            Frame::Control(message) => message,
            _ => bail!("peer sent a non-control response during pairing"),
        };

        let (request_id, server, prompt_message) = match reply {
            ControlMessage::PinChallenge {
                request_id,
                server,
                message,
            } => (request_id, server, message),
            ControlMessage::Error { message } => bail!("{}", message),
            other => bail!("unexpected pairing response: {other:?}"),
        };

        println!();
        println!("{}", style("同步请求已送达").bold());
        println!(
            "对端: {} ({})",
            server.device_name,
            short_uuid(&server.device_id)
        );
        println!("{prompt_message}");
        let pin = prompt_secret("输入服务端当前显示的 6 位 PIN")?;
        let exporter = crypto::export_keying_material_from_client(&client_stream, &request_id)?;
        let proof = crypto::sign_pair_auth(&exporter, &request_id, &pin, &payload)?;
        FrameWriter::new(&mut client_stream)
            .write_frame(Frame::Control(ControlMessage::PairAuth {
                request_id: request_id.clone(),
                proof,
            }))
            .await?;

        let reply = match FrameReader::new(&mut client_stream).read_frame().await? {
            Frame::Control(message) => message,
            _ => bail!("peer sent a non-control response during pairing"),
        };

        match &reply {
            ControlMessage::PairDecision { accepted, .. } => {
                crypto::verify_pair_decision(&reply, &exporter, &request_id, &pin)?;
                if !accepted && let ControlMessage::PairDecision { message, .. } = &reply {
                    bail!("{}", message);
                }
            }
            ControlMessage::Error { message } => bail!("{}", message),
            other => bail!("unexpected pairing response: {other:?}"),
        }

        if let ControlMessage::PairDecision {
            server,
            workspace,
            agreement,
            ..
        } = reply
        {
            Ok::<_, anyhow::Error>((server, workspace, agreement))
        } else {
            bail!("peer did not send a pair decision")
        }
    }?;

    let (remote, remote_workspace, agreement) = remote_info;
    let tls_stream: TlsStream<TcpStream> = client_stream.into();
    println!();
    println!("{}", style("连接已建立").bold());
    println!(
        "对端: {} ({})",
        remote.device_name,
        short_uuid(&remote.device_id)
    );
    println!(
        "协商结果: {}",
        agreement_label(SessionRole::Client, &agreement)
    );

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
            Frame::Control(ControlMessage::PairRequest { .. })
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

fn choose_peer() -> Result<DiscoveredPeer> {
    loop {
        let peers = discovery::browse(Duration::from_secs(3))?;
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
    let proof = crypto::sign_pair_decision(
        params.exporter,
        params.request_id,
        params.pin,
        params.accepted,
        &params.message,
        params.agreement,
        &summary,
    )?;
    Ok(ControlMessage::PairDecision {
        accepted: params.accepted,
        message: params.message,
        server: device_identity(params.device),
        workspace: summary,
        agreement: params.agreement.clone(),
        proof,
    })
}

fn device_identity(device: &DeviceConfig) -> DeviceIdentity {
    DeviceIdentity {
        device_id: device.device_id,
        device_name: device.device_name.clone(),
    }
}

fn print_host_ready(device: &DeviceConfig, options: &RuntimeOptions, port: u16) {
    println!("{}", style("Synly 已就绪").bold());
    println!("设备: {} ({})", device.device_name, device.short_id());
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
    println!("监听端口: {}", port);
    println!("等待同步请求。收到请求后会为该请求单独显示 6 位 PIN。");
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
    use super::delete_policy;
    use crate::sync::{DeletePolicy, SnapshotLayout};

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
}
