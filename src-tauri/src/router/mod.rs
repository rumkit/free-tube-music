pub mod config;

use config::RouterConfig;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_socks::tcp::Socks5Stream;

/// Binds the router's listening socket. Split out from `serve` so callers (namely
/// app setup) can surface a bind failure — e.g. the port falling inside a Windows
/// TCP excluded-port range (WSAEACCES / os error 10013) — before the window is
/// built with a proxy_url pointing at a port nothing is listening on.
pub fn bind(port: u16) -> std::io::Result<std::net::TcpListener> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Runs the local CONNECT router accept loop until the listener fails. Intended to
/// be spawned as a background tokio task for the lifetime of the app.
pub async fn serve(
    listener: std::net::TcpListener,
    config_rx: watch::Receiver<Arc<RouterConfig>>,
) -> std::io::Result<()> {
    let listener = TcpListener::from_std(listener)?;
    log::info!(
        "router listening on {}",
        listener.local_addr().map(|a| a.to_string()).unwrap_or_default()
    );

    loop {
        let (socket, _addr) = listener.accept().await?;
        let config_rx = config_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(socket, config_rx).await {
                log::debug!("router connection error: {e}");
            }
        });
    }
}

async fn handle_connection(
    mut client: TcpStream,
    config_rx: watch::Receiver<Arc<RouterConfig>>,
) -> std::io::Result<()> {
    let (host, port) = match read_connect_target(&mut client).await? {
        Some(target) => target,
        None => {
            client
                .write_all(b"HTTP/1.1 501 Not Implemented\r\n\r\n")
                .await?;
            return Ok(());
        }
    };

    let config = config_rx.borrow().clone();

    let upstream_result = if config.should_proxy(&host) {
        dial_via_socks5(&config, &host, port).await
    } else {
        TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|e| e.to_string())
    };

    let mut upstream = match upstream_result {
        Ok(stream) => stream,
        Err(e) => {
            log::warn!("failed to connect to {host}:{port}: {e}");
            client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
                .await?;
            return Ok(());
        }
    };

    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;

    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

async fn dial_via_socks5(
    config: &RouterConfig,
    host: &str,
    port: u16,
) -> Result<TcpStream, String> {
    let upstream = config
        .upstream
        .as_ref()
        .ok_or_else(|| "no upstream SOCKS5 proxy configured".to_string())?;

    let proxy_addr = (upstream.host.as_str(), upstream.port);
    let target = (host, port);

    let stream = Socks5Stream::connect_with_password(
        proxy_addr,
        target,
        &upstream.username,
        &upstream.password,
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(stream.into_inner())
}

/// Reads the CONNECT request line and headers off `client`, returning the
/// (host, port) target. Returns `Ok(None)` for any non-CONNECT request.
async fn read_connect_target(client: &mut TcpStream) -> std::io::Result<Option<(String, u16)>> {
    let mut buf = Vec::with_capacity(512);
    let mut byte = [0u8; 1];

    // Read until we see the end of the header block ("\r\n\r\n"), byte by byte.
    // CONNECT requests have no body, so this is sufficient and keeps the parser
    // simple without pulling in a full HTTP parsing crate.
    loop {
        let n = client.read(&mut byte).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.push(byte[0]);
        if buf.len() >= 4 && &buf[buf.len() - 4..] == b"\r\n\r\n" {
            break;
        }
        if buf.len() > 8192 {
            return Ok(None);
        }
    }

    let text = String::from_utf8_lossy(&buf);
    let first_line = text.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    if !method.eq_ignore_ascii_case("CONNECT") {
        return Ok(None);
    }

    match target.rsplit_once(':') {
        Some((host, port_str)) => match port_str.parse::<u16>() {
            Ok(port) => Ok(Some((host.to_string(), port))),
            Err(_) => Ok(None),
        },
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::RedirectMode;
    use std::collections::HashSet;
    use tokio::io::AsyncReadExt;

    async fn spawn_router(config: RouterConfig) -> (u16, watch::Sender<Arc<RouterConfig>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = watch::channel(Arc::new(config));

        tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let rx = rx.clone();
                tokio::spawn(handle_connection(socket, rx));
            }
        });

        (port, tx)
    }

    #[tokio::test]
    async fn non_gated_host_is_dialed_directly() {
        // Echo server standing in for the "destination" the router should reach
        // directly (no upstream proxy configured at all, so a direct path is the
        // only way this can succeed).
        let echo_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let echo_port = echo_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = echo_listener.accept().await.unwrap();
            let mut buf = [0u8; 5];
            sock.read_exact(&mut buf).await.unwrap();
            sock.write_all(&buf).await.unwrap();
        });

        let config = RouterConfig {
            mode: RedirectMode::List(HashSet::new()),
            upstream: None,
        };
        let (router_port, _tx) = spawn_router(config).await;

        let mut client = TcpStream::connect(("127.0.0.1", router_port)).await.unwrap();
        client
            .write_all(format!("CONNECT 127.0.0.1:{echo_port} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();

        let expected = b"HTTP/1.1 200 Connection Established\r\n\r\n";
        let mut response = vec![0u8; expected.len()];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, expected);

        client.write_all(b"hello").await.unwrap();
        let mut echoed = [0u8; 5];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"hello");
    }

    #[tokio::test]
    async fn gated_host_without_upstream_returns_bad_gateway() {
        let mut hosts = HashSet::new();
        hosts.insert("gated.example".to_string());
        let config = RouterConfig {
            mode: RedirectMode::List(hosts),
            upstream: None,
        };
        let (router_port, _tx) = spawn_router(config).await;

        let mut client = TcpStream::connect(("127.0.0.1", router_port)).await.unwrap();
        client
            .write_all(b"CONNECT gated.example:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();

        let mut response = vec![0u8; 15];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"HTTP/1.1 502 Ba");
    }
}
