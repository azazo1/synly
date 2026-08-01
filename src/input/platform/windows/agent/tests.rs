use super::client::{
    AgentBackend, AgentClient, ClientCommand, ClientCursorState, ClientQueueItem, CursorUpdate,
    client_event_reader_loop, command_writer_loop, current_client, request_elevation,
    wait_for_agent_response, write_client_command,
};
use super::pipe::{NativePipe, PipeDirection};
use super::protocol::{
    AgentRequest, AgentResponse, AgentToGuiPacket, GuiToAgentPacket, IPC_MAX_FRAME,
    is_timeout_error, read_packet,
};
use super::security::PipeSecurity;
use super::server::{
    AgentHeartbeat, AgentMotion, AgentMotionSlot, AgentOutput, agent_event_writer_loop,
};
use super::{AGENT_HEARTBEAT_TIMEOUT, CONNECT_TIMEOUT, REQUEST_DELIVERY_TIMEOUT};
use super::super::super::NativeEvent;
use crate::input::{DesktopLayout, DisplayRect, Point};
use anyhow::{Result, bail};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

fn test_layout() -> DesktopLayout {
    DesktopLayout::new(vec![DisplayRect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    }])
    .unwrap()
}

fn test_cursor() -> Arc<ClientCursorState> {
    Arc::new(ClientCursorState {
        latest: Mutex::new(None),
        queued: AtomicBool::new(false),
    })
}

fn test_client(
    commands: std::sync::mpsc::SyncSender<ClientQueueItem>,
    alive: Arc<AtomicBool>,
) -> AgentClient {
    AgentClient {
        commands,
        cursor: test_cursor(),
        context: Arc::new(Mutex::new(None)),
        alive,
        lifecycle: Mutex::new(()),
        next_lease: AtomicU64::new(1),
        active_lease: AtomicU64::new(0),
    }
}

fn create_test_server(name: &str, direction: PipeDirection) -> NativePipe {
    let security = PipeSecurity::for_current_user().unwrap();
    NativePipe::create_server(name, direction, &security.attributes).unwrap()
}

#[test]
fn closed_command_queue_marks_agent_unavailable() {
    let (commands, receiver) = std::sync::mpsc::sync_channel(1);
    drop(receiver);
    let alive = Arc::new(AtomicBool::new(true));
    let client = test_client(commands, Arc::clone(&alive));

    assert!(client.request(AgentRequest::Health).is_err());
    assert!(!alive.load(Ordering::Acquire));
}

#[test]
fn stale_backend_drop_does_not_stop_current_lease() {
    let (commands, receiver) = std::sync::mpsc::sync_channel(4);
    let client = Arc::new(test_client(commands, Arc::new(AtomicBool::new(true))));
    client.next_lease.store(3, Ordering::Release);
    client.active_lease.store(2, Ordering::Release);
    drop(AgentBackend {
        client: Arc::clone(&client),
        lease: 1,
        layout: test_layout(),
    });

    assert_eq!(client.active_lease.load(Ordering::Acquire), 2);
    assert!(receiver.try_recv().is_err());
}

#[test]
fn current_backend_drop_stops_runtime_without_closing_reusable_agent() {
    let (commands, receiver) = std::sync::mpsc::sync_channel(4);
    let client = Arc::new(test_client(commands, Arc::new(AtomicBool::new(true))));
    client.next_lease.store(2, Ordering::Release);
    client.active_lease.store(1, Ordering::Release);
    drop(AgentBackend {
        client: Arc::clone(&client),
        lease: 1,
        layout: test_layout(),
    });

    let item = receiver.try_recv().unwrap();
    let ClientQueueItem::Command(command) = item else {
        panic!("expected Stop command");
    };
    assert!(matches!(command.request, AgentRequest::Stop));
    assert!(command.response.is_none());
    assert_eq!(client.active_lease.load(Ordering::Acquire), 0);
    assert!(client.alive.load(Ordering::Acquire));
}

#[test]
fn cursor_notifications_keep_only_the_latest_value() {
    let (commands, receiver) = std::sync::mpsc::sync_channel(1);
    let client = test_client(commands, Arc::new(AtomicBool::new(true)));

    for x in 0..20_000 {
        client
            .notify(AgentRequest::InjectCursor(Point { x, y: 10 }))
            .unwrap();
    }

    assert!(matches!(receiver.try_recv().unwrap(), ClientQueueItem::Cursor));
    assert!(receiver.try_recv().is_err());
    let latest = client.cursor.latest.lock().unwrap().unwrap();
    assert_eq!(latest.point, Point { x: 19_999, y: 10 });
}

#[test]
fn motion_notifications_are_queued_in_fifo_order() {
    let (commands, receiver) = std::sync::mpsc::sync_channel(64);
    let client = test_client(commands, Arc::new(AtomicBool::new(true)));

    for i in 0..10_000 {
        client
            .notify(AgentRequest::InjectMotion { dx: i, dy: -i })
            .unwrap();
        let item = receiver.try_recv().unwrap();
        let ClientQueueItem::Command(command) = item else {
            panic!("motion 通知应进入命令队列");
        };
        match command.request {
            AgentRequest::InjectMotion { dx, dy } => {
                assert_eq!(dx, i);
                assert_eq!(dy, -i);
            }
            other => panic!("队列中不应出现其他请求: {other:?}"),
        }
    }
}

#[test]
fn native_dual_pipe_transport_survives_cursor_lifecycle_and_event_pressure() {
    const CYCLES: usize = 1000;
    const EXPECTED_REQUESTS: usize = 1 + CYCLES * 2 + 2;

    let connection_id = Uuid::new_v4();
    let command_name = format!(r"\\.\pipe\synly-test-command-{connection_id}");
    let event_name = format!(r"\\.\pipe\synly-test-event-{connection_id}");
    let alive = Arc::new(AtomicBool::new(true));
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let context = Arc::new(Mutex::new(None));
    let cursor = test_cursor();
    let (commands, command_rx) = std::sync::mpsc::sync_channel(64);
    let (created_tx, created_rx) = std::sync::mpsc::sync_channel(2);

    let command_pending = Arc::clone(&pending);
    let command_alive = Arc::clone(&alive);
    let command_cursor = Arc::clone(&cursor);
    let command_created = created_tx.clone();
    let command_server_name = command_name.clone();
    let gui_command = std::thread::spawn(move || -> Result<()> {
        let mut pipe = create_test_server(&command_server_name, PipeDirection::ServerToClient);
        command_created.send(()).unwrap();
        pipe.connect_server(CONNECT_TIMEOUT)?;
        command_writer_loop(
            pipe,
            command_rx,
            command_cursor,
            command_pending,
            command_alive,
        )
    });

    let event_pending = Arc::clone(&pending);
    let event_alive = Arc::clone(&alive);
    let event_context = Arc::clone(&context);
    let event_created = created_tx;
    let event_server_name = event_name.clone();
    let gui_event = std::thread::spawn(move || -> Result<()> {
        let mut pipe = create_test_server(&event_server_name, PipeDirection::ClientToServer);
        event_created.send(()).unwrap();
        pipe.connect_server(CONNECT_TIMEOUT)?;
        client_event_reader_loop(pipe, event_pending, event_context, event_alive)
    });

    created_rx.recv_timeout(CONNECT_TIMEOUT).unwrap();
    created_rx.recv_timeout(CONNECT_TIMEOUT).unwrap();

    let (reliable_tx, reliable_rx) = std::sync::mpsc::sync_channel(256);
    let motion = Arc::new(AgentMotionSlot {
        latest: Mutex::new(None),
        changed: AtomicBool::new(false),
    });
    let output = AgentOutput {
        reliable: reliable_tx,
        motion: Arc::clone(&motion),
    };
    let agent_event_alive = Arc::clone(&alive);
    let agent_event_name = event_name;
    let agent_event = std::thread::spawn(move || -> Result<()> {
        let pipe = NativePipe::connect_client(
            &agent_event_name,
            PipeDirection::ClientToServer,
            CONNECT_TIMEOUT,
        )?;
        agent_event_writer_loop(pipe, reliable_rx, motion, agent_event_alive)
    });

    let agent_output = output.clone();
    let agent_command_name = command_name;
    let agent_command = std::thread::spawn(move || -> Result<Vec<u64>> {
        let mut pipe = NativePipe::connect_client(
            &agent_command_name,
            PipeDirection::ServerToClient,
            CONNECT_TIMEOUT,
        )?;
        let mut ids = Vec::with_capacity(EXPECTED_REQUESTS);
        for expected_id in 1..=u64::try_from(EXPECTED_REQUESTS).unwrap() {
            let packet = read_packet::<GuiToAgentPacket>(&mut pipe, REQUEST_DELIVERY_TIMEOUT)?;
            let GuiToAgentPacket::Request { id, request } = packet else {
                bail!("expected request packet");
            };
            assert_eq!(id, expected_id);
            if expected_id == 1 {
                assert!(matches!(
                    request,
                    AgentRequest::InjectCursor(Point { x: 19_999, y: 720 })
                ));
            } else if expected_id == u64::try_from(EXPECTED_REQUESTS - 1).unwrap() {
                assert!(matches!(
                    request,
                    AgentRequest::InjectButton {
                        button: 1,
                        down: true,
                    }
                ));
            } else if expected_id == u64::try_from(EXPECTED_REQUESTS).unwrap() {
                assert!(matches!(
                    request,
                    AgentRequest::InjectWheel { x: 0, y: 120 }
                ));
            }
            ids.push(id);
            agent_output.send_reliable(AgentToGuiPacket::Event(NativeEvent::Button {
                button: 1,
                down: false,
            }))?;
            agent_output.store_motion(AgentMotion {
                dx: 1,
                dy: -1,
                position: Some(Point { x: 10, y: 20 }),
                position_updated: true,
            });
            agent_output.send_reliable(AgentToGuiPacket::Response {
                id,
                response: AgentResponse::Ok,
            })?;
        }
        Ok(ids)
    });

    let producer_commands = commands.clone();
    let producer_cursor = Arc::clone(&cursor);
    let producer = std::thread::spawn(move || {
        for x in 0..20_000 {
            *producer_cursor.latest.lock().unwrap() = Some(CursorUpdate {
                point: Point { x, y: 720 },
            });
        }
        producer_cursor.queued.store(true, Ordering::Release);
        producer_commands.send(ClientQueueItem::Cursor).unwrap();
        for cycle in 0..CYCLES {
            producer_commands
                .send(ClientQueueItem::Command(ClientCommand {
                    request: AgentRequest::ReleaseAll,
                    dispatched: None,
                    response: None,
                }))
                .unwrap();
            producer_commands
                .send(ClientQueueItem::Command(ClientCommand {
                    request: AgentRequest::WarpCursor(Point {
                        x: 2551,
                        y: i32::try_from(cycle % 1440).unwrap(),
                    }),
                    dispatched: None,
                    response: None,
                }))
                .unwrap();
        }
        producer_commands
            .send(ClientQueueItem::Command(ClientCommand {
                request: AgentRequest::InjectButton {
                    button: 1,
                    down: true,
                },
                dispatched: None,
                response: None,
            }))
            .unwrap();
        let (dispatch_tx, dispatch_rx) = std::sync::mpsc::sync_channel(1);
        let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
        producer_commands
            .send(ClientQueueItem::Command(ClientCommand {
                request: AgentRequest::InjectWheel { x: 0, y: 120 },
                dispatched: Some(dispatch_tx),
                response: Some(response_tx),
            }))
            .unwrap();
        (dispatch_rx, response_rx)
    });

    let (dispatch_rx, response_rx) = producer.join().unwrap();
    assert!(matches!(
        wait_for_agent_response(
            "InjectWheel",
            dispatch_rx,
            response_rx,
            Duration::from_secs(30),
        )
        .unwrap(),
        AgentResponse::Ok
    ));
    let ids = agent_command.join().unwrap().unwrap();
    assert_eq!(ids.len(), EXPECTED_REQUESTS);
    assert!(ids.windows(2).all(|pair| pair[1] == pair[0] + 1));

    alive.store(false, Ordering::Release);
    drop(commands);
    drop(output);
    gui_command.join().unwrap().unwrap();
    agent_event.join().unwrap().unwrap();
    gui_event.join().unwrap().unwrap();
}

#[test]
fn native_pipe_rejects_invalid_frame_length() {
    let name = format!(r"\\.\pipe\synly-test-length-{}", Uuid::new_v4());
    let (created_tx, created_rx) = std::sync::mpsc::sync_channel(1);
    let server_name = name.clone();
    let server = std::thread::spawn(move || {
        let mut pipe = create_test_server(&server_name, PipeDirection::ClientToServer);
        created_tx.send(()).unwrap();
        pipe.connect_server(CONNECT_TIMEOUT).unwrap();
        read_packet::<GuiToAgentPacket>(&mut pipe, Duration::from_secs(1))
    });
    created_rx.recv_timeout(CONNECT_TIMEOUT).unwrap();
    let mut client = NativePipe::connect_client(
        &name,
        PipeDirection::ClientToServer,
        CONNECT_TIMEOUT,
    )
    .unwrap();
    client
        .write_all(
            &(u32::try_from(IPC_MAX_FRAME).unwrap() + 1).to_be_bytes(),
            Duration::from_secs(1),
        )
        .unwrap();

    assert!(server.join().unwrap().is_err());
}

#[test]
fn native_pipe_reports_half_frame_when_peer_exits() {
    let name = format!(r"\\.\pipe\synly-test-half-frame-{}", Uuid::new_v4());
    let (created_tx, created_rx) = std::sync::mpsc::sync_channel(1);
    let server_name = name.clone();
    let server = std::thread::spawn(move || {
        let mut pipe = create_test_server(&server_name, PipeDirection::ClientToServer);
        created_tx.send(()).unwrap();
        pipe.connect_server(CONNECT_TIMEOUT).unwrap();
        read_packet::<GuiToAgentPacket>(&mut pipe, Duration::from_secs(1))
    });
    created_rx.recv_timeout(CONNECT_TIMEOUT).unwrap();
    let mut client = NativePipe::connect_client(
        &name,
        PipeDirection::ClientToServer,
        CONNECT_TIMEOUT,
    )
    .unwrap();
    client
        .write_all(&16u32.to_be_bytes(), Duration::from_secs(1))
        .unwrap();
    client
        .write_all(&[1, 2, 3], Duration::from_secs(1))
        .unwrap();
    drop(client);

    assert!(server.join().unwrap().is_err());
}

#[test]
fn native_pipe_read_timeout_is_cancelled() {
    let name = format!(r"\\.\pipe\synly-test-timeout-{}", Uuid::new_v4());
    let (created_tx, created_rx) = std::sync::mpsc::sync_channel(1);
    let server_name = name.clone();
    let server = std::thread::spawn(move || {
        let mut pipe = create_test_server(&server_name, PipeDirection::ClientToServer);
        created_tx.send(()).unwrap();
        pipe.connect_server(CONNECT_TIMEOUT).unwrap();
        let mut byte = [0u8; 1];
        pipe.read_exact(&mut byte, Duration::from_millis(50))
    });
    created_rx.recv_timeout(CONNECT_TIMEOUT).unwrap();
    let _client = NativePipe::connect_client(
        &name,
        PipeDirection::ClientToServer,
        CONNECT_TIMEOUT,
    )
    .unwrap();

    let error = server.join().unwrap().unwrap_err();
    assert!(is_timeout_error(&error));
}

#[test]
fn reliable_request_timeout_keeps_transport_alive() {
    let name = format!(
        r"\\.\pipe\synly-test-response-timeout-{}",
        Uuid::new_v4()
    );
    let (created_tx, created_rx) = std::sync::mpsc::sync_channel(1);
    let alive = Arc::new(AtomicBool::new(true));
    let server_name = name.clone();
    let server = std::thread::spawn(move || {
        let mut pipe = create_test_server(&server_name, PipeDirection::ServerToClient);
        created_tx.send(()).unwrap();
        pipe.connect_server(CONNECT_TIMEOUT).unwrap();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let mut next_id = 1;
        write_client_command(
            &mut pipe,
            ClientCommand {
                request: AgentRequest::Stop,
                dispatched: None,
                response: None,
            },
            &pending,
            &mut next_id,
        )
    });
    created_rx.recv_timeout(CONNECT_TIMEOUT).unwrap();
    let mut client = NativePipe::connect_client(
        &name,
        PipeDirection::ServerToClient,
        CONNECT_TIMEOUT,
    )
    .unwrap();
    let packet = read_packet::<GuiToAgentPacket>(&mut client, REQUEST_DELIVERY_TIMEOUT).unwrap();
    assert!(matches!(
        packet,
        GuiToAgentPacket::Request {
            id: 1,
            request: AgentRequest::Stop,
        }
    ));

    assert!(server.join().unwrap().is_ok());
    assert!(alive.load(Ordering::Acquire));
}

#[test]
fn agent_heartbeat_only_expires_after_the_full_timeout() {
    let started = Instant::now();
    let heartbeat = AgentHeartbeat::new(started);

    assert!(!heartbeat.expired(started + AGENT_HEARTBEAT_TIMEOUT));
    assert!(heartbeat.expired(
        started + AGENT_HEARTBEAT_TIMEOUT + Duration::from_millis(1)
    ));
}

#[test]
#[ignore = "requires interactive UAC approval and a real Windows desktop"]
fn elevated_agent_process_is_reused_across_requests() {
    request_elevation().unwrap();
    let first = current_client().unwrap();
    request_elevation().unwrap();
    let second = current_client().unwrap();

    assert!(Arc::ptr_eq(&first, &second));
}
