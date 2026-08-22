use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::runtime::Handle;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

/// Start one bounded Tokio runtime shared by downstream and upstream HTTP/3.
/// Both directions are QUIC event loops, so separate permanent schedulers only
/// add worker stacks, timer drivers, and cross-runtime wakeups.
pub fn start(worker_threads: usize) -> Result<Handle> {
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<Handle, String>>(1);
    thread::Builder::new()
        .name("jbs-http3".to_string())
        .spawn(move || {
            let worker_threads = worker_threads.clamp(1, 8);
            let runtime = if worker_threads <= 1 {
                tokio::runtime::Builder::new_current_thread()
                    .thread_name("jbs-h3-worker")
                    .enable_all()
                    .build()
            } else {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(worker_threads)
                    .thread_name("jbs-h3-worker")
                    .enable_all()
                    .build()
            };
            let runtime = match runtime {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready_tx.send(Err(format!("HTTP/3 runtime creation failed: {error}")));
                    return;
                }
            };
            if ready_tx.send(Ok(runtime.handle().clone())).is_err() {
                return;
            }
            runtime.block_on(std::future::pending::<()>());
        })
        .context("failed to spawn shared HTTP/3 runtime thread")?;

    ready_rx
        .recv_timeout(STARTUP_TIMEOUT)
        .map_err(|error| anyhow!("shared HTTP/3 runtime startup did not complete: {error}"))?
        .map_err(anyhow::Error::msg)
}
