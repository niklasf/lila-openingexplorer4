//! Listening on the public address and, optionally, on a separate address
//! for administration and monitoring.

use std::{io, net::SocketAddr};

use axum::Router;
use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    sync::watch,
};

async fn shutdown_signal() {
    let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => log::info!("received SIGINT, shutting down"),
        _ = sigterm.recv() => log::info!("received SIGTERM, shutting down"),
    }
}

/// Serves `app` on `bind` and `admin` on its own address (if any) until
/// SIGINT or SIGTERM. Both addresses are bound before serving either.
pub async fn serve_until_signal(
    bind: SocketAddr,
    app: Router,
    admin: Option<(SocketAddr, Router)>,
) -> io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    let admin = match admin {
        Some((addr, router)) => Some((TcpListener::bind(addr).await?, addr, router)),
        None => None,
    };
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = tx.send(true);
    });
    let stopped = |mut rx: watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|stop| *stop).await;
    };
    log::info!("listening on {bind}");
    let public = axum::serve(listener, app).with_graceful_shutdown(stopped(rx.clone()));
    match admin {
        Some((listener, addr, router)) => {
            log::info!("administration and monitoring on {addr}");
            let admin = axum::serve(listener, router).with_graceful_shutdown(stopped(rx));
            tokio::try_join!(public.into_future(), admin.into_future())?;
        }
        None => {
            log::info!("no administration address, monitoring disabled");
            public.await?;
        }
    }
    Ok(())
}
