use std::time::Duration;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::device::Device;

pub mod announcement;
pub mod pcm;

/// Wait until discovery supplies a device or the caller should stop.
async fn wait_for_device(
    device_rx: &mut watch::Receiver<Option<Device>>,
    shutdown: &CancellationToken,
) -> Option<Device> {
    loop {
        if shutdown.is_cancelled() {
            return None;
        }
        if let Some(device) = device_rx.borrow().clone() {
            return Some(device);
        }

        tokio::select! {
            changed = device_rx.changed() => {
                if changed.is_err() {
                    return None;
                }
            }
            _ = shutdown.cancelled() => return None,
        }
    }
}

async fn wait_to_retry(delay: Duration, shutdown: &CancellationToken) {
    tokio::select! {
        _ = tokio::time::sleep(delay) => {}
        _ = shutdown.cancelled() => {}
    }
}
