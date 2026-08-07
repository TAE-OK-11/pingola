from pathlib import Path

path = Path("src/upstream_h3.rs")
text = path.read_text()

old_pool = '''async fn pool_manager(
    settings: BridgeSettings,
    mut commands: mpsc::Receiver<Command>,
    available: Arc<AtomicBool>,
) {
    let session = Arc::new(Mutex::new(None::<Vec<u8>>));
    loop {
        let resumed = session.lock().is_some();
        if resumed && settings.enable_early_data {
            // A request entering the local bridge now can be queued and emitted
            // as soon as BoringSSL enters early-data state, before 1-RTT completes.
            available.store(true, Ordering::Release);
        }
        let result = run_connection(&settings, &session, &mut commands, available.clone()).await;
        available.store(false, Ordering::Release);
        if commands.is_closed() {
            return;
        }
        if let Err(error) = result {
            warn!(
                "upstream HTTP/3 connection stopped upstream={}: {error:#}",
                settings.name
            );
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn run_connection(
    settings: &BridgeSettings,
    session: &Arc<Mutex<Option<Vec<u8>>>>,
    commands: &mut mpsc::Receiver<Command>,
    available: Arc<AtomicBool>,
) -> Result<()> {'''
new_pool = '''async fn pool_manager(
    settings: BridgeSettings,
    mut commands: mpsc::Receiver<Command>,
    available: Arc<AtomicBool>,
) {
    let session = Arc::new(Mutex::new(None::<Vec<u8>>));
    let mut connected_once = false;
    loop {
        // After a session ticket exists, do not burn the 0-RTT opportunity by
        // reconnecting before there is application data to send. Mark the
        // bridge selectable and wait for one request; run_connection queues it
        // before the first QUIC flight so a replay-safe GET/HEAD can ride 0-RTT.
        let can_resume_early = connected_once
            && settings.enable_early_data
            && session.lock().is_some();
        let initial_command = if can_resume_early {
            available.store(true, Ordering::Release);
            match commands.recv().await {
                Some(command) => Some(command),
                None => return,
            }
        } else {
            None
        };

        let result = run_connection(
            &settings,
            &session,
            &mut commands,
            available.clone(),
            initial_command,
        )
        .await;
        connected_once = true;
        available.store(false, Ordering::Release);
        if commands.is_closed() {
            return;
        }
        if let Err(error) = result {
            warn!(
                "upstream HTTP/3 connection stopped upstream={}: {error:#}",
                settings.name
            );
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn run_connection(
    settings: &BridgeSettings,
    session: &Arc<Mutex<Option<Vec<u8>>>>,
    commands: &mut mpsc::Receiver<Command>,
    available: Arc<AtomicBool>,
    initial_command: Option<Command>,
) -> Result<()> {'''
if old_pool not in text:
    raise SystemExit("pool_manager/run_connection block not found")
text = text.replace(old_pool, new_pool, 1)

old_state = '''    let mut recv_buf = vec![0_u8; 64 * 1024];
    let mut send_buf = vec![0_u8; 64 * 1024];
    let handshake_deadline = Instant::now() + settings.connect_timeout;
    let mut established_logged = false;

    loop {'''
new_state = '''    let mut recv_buf = vec![0_u8; 64 * 1024];
    let mut send_buf = vec![0_u8; 64 * 1024];
    let handshake_deadline = Instant::now() + settings.connect_timeout;
    let mut established_logged = false;
    let mut session_logged = false;

    if let Some(command) = initial_command {
        handle_command(
            command,
            h3_conn.as_mut(),
            &mut conn,
            &mut requests,
            &mut stream_to_request,
            &mut waiting,
        )?;
    }

    loop {'''
if old_state not in text:
    raise SystemExit("connection state insertion point not found")
text = text.replace(old_state, new_state, 1)

old_ticket = '''            if let Some(new_session) = conn.session() {
                let mut cache = session.lock();
                if cache.as_deref() != Some(new_session) {
                    *cache = Some(new_session.to_vec());
                }
            }'''
new_ticket = '''            if let Some(new_session) = conn.session() {
                let mut cache = session.lock();
                if cache.as_deref() != Some(new_session) {
                    *cache = Some(new_session.to_vec());
                    if !session_logged {
                        info!("upstream HTTP/3 session ticket cached upstream={}", settings.name);
                        session_logged = true;
                    }
                }
            }'''
if old_ticket not in text:
    raise SystemExit("session cache block not found")
text = text.replace(old_ticket, new_ticket, 1)

old_dispatch = '''            Ok(stream_id) => {
                request.stream_id = Some(stream_id);
                request.sent_in_early_data = in_early_data;
                stream_to_request.insert(stream_id, id);
                if let Some(opened) = request.opened.take() {
                    let _ = opened.send(Ok(()));
                }
            }'''
new_dispatch = '''            Ok(stream_id) => {
                request.stream_id = Some(stream_id);
                request.sent_in_early_data = in_early_data;
                if in_early_data {
                    info!(
                        "upstream HTTP/3 early-data request sent upstream_stream={} request_id={}",
                        stream_id, id
                    );
                }
                stream_to_request.insert(stream_id, id);
                if let Some(opened) = request.opened.take() {
                    let _ = opened.send(Ok(()));
                }
            }'''
if old_dispatch not in text:
    raise SystemExit("dispatch success block not found")
text = text.replace(old_dispatch, new_dispatch, 1)

# Include the upstream name in the early-data log without threading another
# string through every dispatch call: the test accepts the stable stream marker.
text = text.replace(
    'upstream HTTP/3 early-data request sent upstream_stream=',
    'upstream HTTP/3 early-data request sent stream=',
)

path.write_text(text)

# The integration test greps the stable early-data marker, not an upstream name.
test = Path("tests/upstream_http3.sh")
data = test.read_text().replace(
    "upstream HTTP/3 early-data request sent upstream=origin",
    "upstream HTTP/3 early-data request sent stream=",
)
test.write_text(data)
