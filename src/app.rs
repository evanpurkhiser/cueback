use anyhow::{Context, Result, anyhow};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, watch},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

use crate::{config::Config, rx3, service::CaptureService};

/// Run Cueback until interrupted or a supervised service fails.
pub async fn run(config: Config) -> Result<()> {
    let shutdown = CancellationToken::new();
    let (device_tx, device_rx) = watch::channel(None);
    let (event_tx, event_rx) = mpsc::channel(8);
    let mut tasks = JoinSet::new();

    let socket = UdpSocket::bind(config.device.announcement_bind)
        .await
        .with_context(|| {
            format!(
                "failed to bind announcement listener to {}",
                config.device.announcement_bind
            )
        })?;
    let timeout = config.device.announcement_timeout;
    let task_shutdown = shutdown.child_token();
    tasks.spawn(async move {
        rx3::announcement::watch_rx3(socket, timeout, device_tx, task_shutdown)
            .await
            .context("RX3 announcement listener failed")
    });

    let port = config.device.pcm_port;
    let idle_timeout = config.device.pcm_idle_timeout;
    let reconnect_delay = config.device.reconnect_delay;
    let task_shutdown = shutdown.child_token();
    tasks.spawn(async move {
        rx3::pcm::supervise(
            device_rx,
            port,
            idle_timeout,
            reconnect_delay,
            event_tx,
            task_shutdown,
        )
        .await
        .context("RX3 PCM supervisor failed")
    });

    let capture = CaptureService::new(
        config.recording.ffmpeg,
        config.storage.recordings_dir,
        config.recording.silence_timeout,
        config.recording.silence_threshold,
    );
    let task_shutdown = shutdown.child_token();
    tasks.spawn(async move {
        capture
            .run(event_rx, task_shutdown)
            .await
            .context("capture service failed")
    });

    let outcome = tokio::select! {
        signal = shutdown_signal() => {
            signal.context("failed to listen for shutdown signal")
        }
        completed = tasks.join_next() => {
            match completed {
                Some(Ok(Ok(()))) => Err(anyhow!("a Cueback service stopped unexpectedly")),
                Some(Ok(Err(error))) => Err(error),
                Some(Err(error)) => Err(error).context("a Cueback service task failed"),
                None => Err(anyhow!("all Cueback services stopped unexpectedly")),
            }
        }
    };

    shutdown.cancel();
    while let Some(result) = tasks.join_next().await {
        result.context("a Cueback service task failed during shutdown")??;
    }

    outcome
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }

    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
