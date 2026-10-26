//! WebSockets to the server, with timeouts on every step, so a laptop waking from sleep or a network
//! that vanished shows up as an error within seconds instead of a connection that hangs forever.

use std::{
    io,
    net::{TcpStream, ToSocketAddrs},
    time::{Duration, Instant},
};

use serde::de::DeserializeOwned;
use tungstenite::{Message, WebSocket, client::IntoClientRequest, stream::MaybeTlsStream};

pub type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);

pub fn ws_url(server: &str, path: &str) -> String {
    match server.split_once("://") {
        Some(("http", rest)) => format!("ws://{rest}{path}"),
        Some((_, rest)) => format!("wss://{rest}{path}"),
        None => format!("wss://{server}{path}"),
    }
}

pub fn connect(server: &str, path: &str) -> Result<Socket, String> {
    let url = ws_url(server, path);
    let unreachable = |e: &dyn std::fmt::Display| format!("cannot reach {server}: {e}");
    let request = url.as_str().into_client_request().map_err(|e| unreachable(&e))?;
    let host = request
        .uri()
        .host()
        .unwrap_or_default()
        .trim_matches(['[', ']'])
        .to_string();
    let port = request
        .uri()
        .port_u16()
        .unwrap_or(if url.starts_with("wss:") { 443 } else { 80 });
    let addrs = (host.as_str(), port).to_socket_addrs().map_err(|e| unreachable(&e))?;
    let mut last = io::Error::other("no address found");
    let mut stream = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = e,
        }
    }
    let stream = stream.ok_or_else(|| unreachable(&last))?;
    let _ = stream.set_nodelay(true);
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .and_then(|_| stream.set_write_timeout(Some(WRITE_TIMEOUT)))
        .map_err(|e| unreachable(&e))?;
    let (socket, _) = tungstenite::client_tls_with_config(request, stream, None, None).map_err(|e| unreachable(&e))?;
    Ok(socket)
}

pub fn set_read_timeout(socket: &Socket, timeout: Duration) {
    let tcp = match socket.get_ref() {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::Rustls(s) => s.get_ref(),
        _ => return,
    };
    let _ = tcp.set_read_timeout(Some(timeout));
}

pub enum Frame {
    Text(String),
    /// A ping, pong or other frame: proof of life, nothing to handle.
    Other,
    /// Nothing arrived before the read timeout.
    Idle,
}

pub fn read(socket: &mut Socket) -> Result<Frame, String> {
    match socket.read() {
        Ok(Message::Text(t)) => Ok(Frame::Text(t.to_string())),
        Ok(Message::Close(_)) => Err("the server closed the connection".into()),
        Ok(_) => Ok(Frame::Other),
        Err(tungstenite::Error::Io(e))
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(Frame::Idle)
        }
        Err(e) => Err(format!("connection lost: {e}")),
    }
}

pub fn send(socket: &mut Socket, text: String) -> Result<(), String> {
    match socket.send(Message::Text(text.into())) {
        Ok(()) => Ok(()),
        // Queued; tungstenite writes it out on the next read or send.
        Err(tungstenite::Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
        Err(e) => Err(format!("connection lost: {e}")),
    }
}

#[derive(serde::Deserialize)]
struct Tagged {
    t: String,
    #[serde(default)]
    error: String,
}

/// Waits for the next message, failing on `{"t":"error"}`, a closed socket, or `timeout`.
pub fn expect<T: DeserializeOwned>(socket: &mut Socket, timeout: Duration) -> Result<T, String> {
    let deadline = Instant::now() + timeout;
    set_read_timeout(socket, Duration::from_millis(500));
    loop {
        if Instant::now() >= deadline {
            return Err("the other device took too long to answer".into());
        }
        let Frame::Text(text) = read(socket)? else { continue };
        let tagged: Tagged = serde_json::from_str(&text).map_err(|_| "the server sent something unexpected")?;
        if tagged.t == "error" {
            return Err(tagged.error);
        }
        return serde_json::from_str(&text).map_err(|_| "the server sent something unexpected".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_urls_follow_the_server_scheme() {
        assert_eq!(
            ws_url("https://m.example.com", "/clip/ws"),
            "wss://m.example.com/clip/ws"
        );
        assert_eq!(
            ws_url("http://localhost:3000", "/clip/pair"),
            "ws://localhost:3000/clip/pair"
        );
    }
}
