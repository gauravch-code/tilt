//! The stream: tilt's protocol over WebTransport (QUIC, UDP), on the same port as HTTP (docs/protocol.md section 7). A session that arrives here runs exactly like a
//! WebSocket one (src/session.rs); src/transport.rs carries its messages.
//!
//! Browsers learn the port and the certificate from GET /transport. The certificate is
//! self-signed and short-lived, as Chrome requires for `serverCertificateHashes`: no domain or
//! CA is needed, and a new one replaces it every 6 days.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::Serialize;
use tracing::{debug, info, warn};
use wtransport::endpoint::endpoint_side::Server;
use wtransport::endpoint::IncomingSession;
use wtransport::quinn::{IdleTimeout, TransportConfig};
use wtransport::{Endpoint, Identity, ServerConfig, VarInt};

use crate::clock::LogEvery;
use crate::congestion::FlooredBbr;
use crate::protocol;
use crate::server::AppState;
use crate::session;
use crate::transport::{Sink, Stream};

/// The path of the WebTransport session, as for the WebSocket.
pub const STREAM_PATH: &str = "/stream";

/// Chrome accepts a certificate given by hash only if it is valid for at most 14 days. It starts
/// a day back, for clients whose clocks are behind ours.
const CERT_BACKDATE: Duration = Duration::from_secs(24 * 3600);
const CERT_VALIDITY: Duration = Duration::from_secs(14 * 24 * 3600);
/// A new certificate this often, well before the last one runs out. Sessions keep theirs.
const CERT_ROTATE: Duration = Duration::from_secs(6 * 24 * 3600);
/// The QUIC handshake and the session request must be done within this.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// A peer silent this long is gone (clients PING every second; QUIC keeps the path alive), as
/// TCP keepalive drops a WebSocket peer after about 25 s.
const IDLE_TIMEOUT: Duration = Duration::from_secs(25);
const KEEP_ALIVE: Duration = Duration::from_secs(5);
/// Unacknowledged bytes QUIC may have out. The client's ACKs already limit video in flight
/// (src/flow.rs) to far less on any real link; this only bounds a misbehaving one.
const SEND_WINDOW: u64 = 4 * 1024 * 1024;
/// Datagrams QUIC may hold for sending, the oldest dropped beyond it: as much as the window.
/// A frame whose datagrams were dropped is sent again on a stream (src/transport.rs).
const DATAGRAM_BUFFER: usize = 4 * 1024 * 1024;
/// Incoming control-stream messages are limited like WebSocket ones.
const MAX_INCOMING_MSG: usize = 64 * 1024;

/// What GET /transport tells browsers about the stream.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Advert {
    /// The host to dial; absent: the page's own host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The UDP port to dial.
    pub port: u16,
    /// SHA-256 of the certificate, hex, for `serverCertificateHashes`.
    pub hashes: Vec<String>,
    /// Goes in the session URL (`/stream?ticket=`): other sites' pages cannot read /transport,
    /// so a session with it comes from a page this server served.
    pub ticket: String,
}

/// The running endpoint, for `run` and for closing it at shutdown.
pub type WtEndpoint = Arc<Endpoint<Server>>;

/// Binds UDP `addr` and publishes the advert in `state`.
pub fn bind(state: &AppState, addr: SocketAddr) -> anyhow::Result<WtEndpoint> {
    let (config, hash) = server_config(addr)?;
    let endpoint = Endpoint::server(config)
        .map_err(|e| anyhow::anyhow!("cannot listen for WebTransport on udp {addr}: {e}"))?;
    let local = endpoint.local_addr()?;
    let public = state.cfg.udp_public.as_ref();
    let advert = Advert {
        host: public.and_then(|p| p.host.clone()),
        port: public.map_or(local.port(), |p| p.port),
        hashes: vec![hash],
        ticket: ticket(),
    };
    match &advert.host {
        Some(host) => info!(
            "WebTransport on udp {local}, advertised as {host}:{}",
            advert.port
        ),
        None => info!(
            "WebTransport on udp {local}, advertised as port {}",
            advert.port
        ),
    }
    *state.wt_advert() = Some(advert);
    Ok(Arc::new(endpoint))
}

/// Accepts sessions and rotates the certificate until the task is dropped.
pub async fn run(endpoint: WtEndpoint, state: Arc<AppState>) {
    let mut rotate = tokio::time::interval_at((Instant::now() + CERT_ROTATE).into(), CERT_ROTATE);
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                tokio::spawn(accept(incoming, Arc::clone(&state)));
            }
            _ = rotate.tick() => {
                if let Err(e) = rotate_certificate(&endpoint, &state) {
                    warn!("WebTransport: cannot replace the certificate: {e:#}");
                }
            }
        }
    }
}

fn rotate_certificate(endpoint: &Endpoint<Server>, state: &AppState) -> anyhow::Result<()> {
    let addr = endpoint.local_addr()?;
    let (config, hash) = server_config(addr)?;
    endpoint.reload_config(config, false)?;
    if let Some(advert) = state.wt_advert().as_mut() {
        advert.hashes = vec![hash];
    }
    info!("WebTransport: new certificate");
    Ok(())
}

/// A server config with a fresh certificate, and the certificate's hash.
fn server_config(addr: SocketAddr) -> anyhow::Result<(ServerConfig, String)> {
    let start = time::OffsetDateTime::now_utc() - CERT_BACKDATE;
    // Browsers given the hash check only it, not the name.
    let identity = Identity::self_signed_builder()
        .subject_alt_names(["localhost"])
        .validity_period(start, start + CERT_VALIDITY)
        .build()
        .context("creating the WebTransport certificate")?;
    let cert = &identity.certificate_chain().as_slice()[0];
    let hash = cert
        .hash()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    let mut transport = TransportConfig::default();
    // BBR, not loss-based CUBIC: random loss on a long path does not halve the rate. With a
    // floor under its window (src/congestion.rs), below which flow control decides.
    transport
        .congestion_controller_factory(Arc::new(FlooredBbr::default()))
        .max_idle_timeout(Some(IdleTimeout::try_from(IDLE_TIMEOUT)?))
        .keep_alive_interval(Some(KEEP_ALIVE))
        .send_window(SEND_WINDOW)
        .datagram_send_buffer_size(DATAGRAM_BUFFER);
    let config = ServerConfig::builder()
        .with_bind_address(addr)
        .with_custom_transport(identity, transport)
        .build();
    Ok((config, hash))
}

fn ticket() -> String {
    let mut bytes = [0u8; 16];
    // Without entropy the ticket is predictable, which matters only with --no-auth.
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One incoming QUIC connection: the WebTransport session request, its checks, and then the
/// session itself on the client's first bidirectional stream.
async fn accept(incoming: IncomingSession, state: Arc<AppState>) {
    let request = match tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming).await {
        Ok(Ok(request)) => request,
        Ok(Err(e)) => return debug!("WebTransport handshake failed: {e}"),
        Err(_) => return debug!("WebTransport handshake timed out"),
    };
    let (path, query) = request
        .path()
        .split_once('?')
        .unwrap_or((request.path(), ""));
    if path != STREAM_PATH {
        debug!(path, "WebTransport request for an unknown path");
        return request.not_found().await;
    }
    let ticket = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("ticket="))
        .unwrap_or("");
    let expected = state.wt_advert().as_ref().map(|a| a.ticket.clone());
    if expected.as_deref() != Some(ticket) {
        static REFUSALS: Mutex<LogEvery> = Mutex::new(LogEvery::new(Duration::from_secs(10)));
        let log = REFUSALS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ready(Instant::now());
        if let Some(more) = log {
            warn!(
                origin = ?request.origin(),
                "refused a session without this server's ticket ({more} more since the last \
                 warning)"
            );
        }
        return request.forbidden().await;
    }
    let conn = match request.accept().await {
        Ok(conn) => conn,
        Err(e) => return debug!("WebTransport accept failed: {e}"),
    };
    let peer = conn.remote_address();
    let (send, recv) = match tokio::time::timeout(session::HELLO_TIMEOUT, conn.accept_bi()).await {
        Ok(Ok(streams)) => streams,
        Ok(Err(e)) => return debug!(%peer, "WebTransport session ended before its stream: {e}"),
        Err(_) => {
            conn.close(
                VarInt::from_u32(protocol::CLOSE_HELLO_TIMEOUT.into()),
                b"no stream",
            );
            return debug!(%peer, "WebTransport session opened no stream");
        }
    };
    debug!(%peer, "WebTransport session");
    session::run(
        Sink::new(conn.clone(), send),
        Stream::new(conn, recv, MAX_INCOMING_MSG),
        state,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_advert_names_the_certificate_and_the_public_port() {
        let state = crate::server::test_state(&["--no-auth", "--udp-public", "4433"]);
        let endpoint = bind(&state, "127.0.0.1:0".parse().unwrap()).unwrap();
        let advert = state.wt_advert().clone().unwrap();
        assert_eq!((advert.host.as_deref(), advert.port), (None, 4433));
        let json = serde_json::to_string(&advert).unwrap();
        assert!(json.starts_with(r#"{"port":4433,"hashes":["#), "{json}");
        assert_eq!(advert.hashes.len(), 1);
        assert_eq!(advert.hashes[0].len(), 64);
        assert_eq!(advert.ticket.len(), 32);
        rotate_certificate(&endpoint, &state).unwrap();
        let rotated = state.wt_advert().clone().unwrap();
        assert_ne!(rotated.hashes, advert.hashes, "a new certificate");
        assert_eq!((rotated.port, &rotated.ticket), (4433, &advert.ticket));

        // A sandbox's public UDP listener: another host, named in the advert.
        let state =
            crate::server::test_state(&["--no-auth", "--udp-public", "udp.example.net:41234"]);
        let _endpoint = bind(&state, "127.0.0.1:0".parse().unwrap()).unwrap();
        let advert = state.wt_advert().clone().unwrap();
        assert_eq!(
            (advert.host.as_deref(), advert.port),
            (Some("udp.example.net"), 41234)
        );
        let json = serde_json::to_string(&advert).unwrap();
        assert!(
            json.starts_with(r#"{"host":"udp.example.net","port":41234,"#),
            "{json}"
        );
    }

    #[test]
    fn certificates_suit_server_certificate_hashes() {
        let start = time::OffsetDateTime::now_utc() - CERT_BACKDATE;
        let identity = Identity::self_signed_builder()
            .subject_alt_names(["localhost"])
            .validity_period(start, start + CERT_VALIDITY)
            .build()
            .unwrap();
        assert_eq!(identity.certificate_chain().as_slice().len(), 1);
        assert!(CERT_VALIDITY <= Duration::from_secs(14 * 24 * 3600));
        assert!(CERT_ROTATE + CERT_BACKDATE < CERT_VALIDITY);
    }

    use crate::transport::{frame, KIND_BINARY, KIND_CLOSE, KIND_TEXT};
    use wtransport::tls::Sha256Digest;
    use wtransport::{ClientConfig, Connection, RecvStream, SendStream};

    /// A server on a free UDP port with `args`, and its advert.
    async fn start(args: &[&str]) -> (Arc<AppState>, Advert, tokio::task::JoinHandle<()>) {
        let state = crate::server::test_state(args);
        let endpoint = bind(&state, "127.0.0.1:0".parse().unwrap()).unwrap();
        let advert = state.wt_advert().clone().unwrap();
        let task = tokio::spawn(run(endpoint, Arc::clone(&state)));
        (state, advert, task)
    }

    fn stream_path(advert: &Advert) -> String {
        format!("{STREAM_PATH}?ticket={}", advert.ticket)
    }

    /// Connects as a browser given `advert` would, to `path`.
    async fn connect(advert: &Advert, path: &str) -> anyhow::Result<Connection> {
        let hash: [u8; 32] = (0..32)
            .map(|i| u8::from_str_radix(&advert.hashes[0][2 * i..2 * i + 2], 16).unwrap())
            .collect::<Vec<u8>>()
            .try_into()
            .unwrap();
        let config = ClientConfig::builder()
            .with_bind_address("127.0.0.1:0".parse().unwrap())
            .with_server_certificate_hashes([Sha256Digest::new(hash)])
            .build();
        let url = format!("https://127.0.0.1:{}{path}", advert.port);
        Ok(Endpoint::client(config)?.connect(url).await?)
    }

    /// Reads one control-stream frame: (kind, payload).
    async fn read_frame(recv: &mut RecvStream) -> (u8, Vec<u8>) {
        let mut len = [0u8; 4];
        recv.read_exact(&mut len).await.unwrap();
        let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
        recv.read_exact(&mut body).await.unwrap();
        (body[0], body[1..].to_vec())
    }

    async fn read_json(recv: &mut RecvStream) -> serde_json::Value {
        let (kind, body) = read_frame(recv).await;
        assert_eq!(kind, KIND_TEXT);
        serde_json::from_slice(&body).unwrap()
    }

    async fn hello(conn: &Connection, token: &str) -> (SendStream, RecvStream) {
        let (mut send, recv) = conn.open_bi().await.unwrap().await.unwrap();
        let hello = format!(r#"{{"t":"hello","v":1,"token":"{token}","client":"test"}}"#);
        send.write_all(&frame(KIND_TEXT, hello.as_bytes()))
            .await
            .unwrap();
        (send, recv)
    }

    #[tokio::test]
    async fn a_session_over_webtransport_speaks_the_protocol() {
        let (state, advert, server) = start(&["--token", "secret"]).await;
        let conn = connect(&advert, &stream_path(&advert)).await.unwrap();
        let (mut send, mut recv) = hello(&conn, "secret").await;
        let welcome = read_json(&mut recv).await;
        assert_eq!(
            (welcome["t"].as_str(), welcome["role"].as_str()),
            (Some("welcome"), Some("control"))
        );
        assert_eq!(read_json(&mut recv).await["t"], "control");
        assert_eq!(state.sessions.count(), 1);

        // PING comes back as PONG on the control stream.
        send.write_all(&frame(KIND_BINARY, &[protocol::MSG_PING, 7, 0, 0, 0]))
            .await
            .unwrap();
        let (kind, pong) = loop {
            let (kind, body) = read_frame(&mut recv).await;
            if body.first() == Some(&protocol::MSG_PONG) {
                break (kind, body);
            }
        };
        assert_eq!(
            (kind, pong),
            (KIND_BINARY, vec![protocol::MSG_PONG, 7, 0, 0, 0])
        );

        // Taking control is answered like on a WebSocket.
        send.write_all(&frame(KIND_TEXT, br#"{"t":"control","take":true}"#))
            .await
            .unwrap();
        let control = loop {
            let m = read_json(&mut recv).await;
            if m["t"] == "control" {
                break m;
            }
        };
        assert_eq!(control["you"], true);

        // Closing the client's half ends the session.
        send.finish().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), state.sessions.drained())
            .await
            .expect("the session ends");
        server.abort();
    }

    /// The session's frame rate cap for a hello with these extra fields.
    async fn welcome_for(advert: &Advert, extra: &str) -> serde_json::Value {
        let conn = connect(advert, &stream_path(advert)).await.unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap().await.unwrap();
        let hello = format!(r#"{{"t":"hello","v":1{extra}}}"#);
        send.write_all(&frame(KIND_TEXT, hello.as_bytes()))
            .await
            .unwrap();
        read_json(&mut recv).await
    }

    #[tokio::test]
    async fn the_frame_rate_cap_follows_the_viewers_display() {
        let (_state, advert, server) = start(&["--no-auth"]).await;
        // A viewer that names no display gets 60, as before there was a choice.
        assert_eq!(welcome_for(&advert, "").await["fps"], 60);
        assert_eq!(welcome_for(&advert, r#","fps":120"#).await["fps"], 120);
        assert_eq!(welcome_for(&advert, r#","fps":90"#).await["fps"], 90);
        assert_eq!(welcome_for(&advert, r#","fps":240"#).await["fps"], 120);
        assert_eq!(welcome_for(&advert, r#","fps":0"#).await["fps"], 1);
        server.abort();
        let (_state, advert, server) = start(&["--no-auth", "--max-fps", "30"]).await;
        assert_eq!(welcome_for(&advert, "").await["fps"], 30);
        assert_eq!(welcome_for(&advert, r#","fps":120"#).await["fps"], 30);
        server.abort();
    }

    // Two threads: the test waits on the input channel while the session runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn input_datagrams_are_acted_on_once_and_in_order() {
        use crate::input::InputCmd;
        let (state, input) = crate::server::test_state_with_input(&["--no-auth"]);
        let endpoint = bind(&state, "127.0.0.1:0".parse().unwrap()).unwrap();
        let advert = state.wt_advert().clone().unwrap();
        let server = tokio::spawn(run(endpoint, Arc::clone(&state)));
        let conn = connect(&advert, &stream_path(&advert)).await.unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap().await.unwrap();
        // Message 1 on the control stream.
        send.write_all(&frame(KIND_TEXT, br#"{"t":"hello","v":1,"datagrams":1}"#))
            .await
            .unwrap();
        let welcome = read_json(&mut recv).await;
        assert_eq!(welcome["datagrams"], true);
        // Message 2.
        send.write_all(&frame(KIND_TEXT, br#"{"t":"control","take":true}"#))
            .await
            .unwrap();
        loop {
            let m = read_json(&mut recv).await;
            if m["t"] == "control" && m["you"] == true {
                break;
            }
        }
        let key = |down: u8, keysym: u8| vec![protocol::MSG_KEY, down, keysym, 0, 0, 0];
        let run = |first: u32, msgs: &[Vec<u8>]| {
            let mut d = vec![protocol::MSG_INPUT];
            d.extend_from_slice(&first.to_le_bytes());
            for m in msgs {
                d.push(m.len() as u8);
                d.extend_from_slice(m);
            }
            d
        };
        // The next key the input thread is sent (taking control tells it other things).
        let next = |what: &str| loop {
            let cmd = input
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("no input for {what}"));
            if let InputCmd::Key { keysym, down, .. } = cmd {
                return (keysym, down);
            }
        };
        let no_key = || !input.try_iter().any(|c| matches!(c, InputCmd::Key { .. }));
        // Messages 3 and 4 come as a datagram before the stream has them.
        conn.send_datagram(run(3, &[key(1, 0x61), key(0, 0x61)]))
            .unwrap();
        assert_eq!(next("the datagram"), (0x61, true));
        assert_eq!(next("the datagram"), (0x61, false));
        // An ACK and a PING on the stream are not numbered; then the stream's own copies of
        // 3 and 4, which change nothing, and 5, which only the stream brings.
        send.write_all(&frame(KIND_BINARY, &[protocol::MSG_PING, 1, 0, 0, 0]))
            .await
            .unwrap();
        for msg in [key(1, 0x61), key(0, 0x61), key(1, 0x62)] {
            send.write_all(&frame(KIND_BINARY, &msg)).await.unwrap();
        }
        assert_eq!(next("message 5"), (0x62, true));
        // A datagram repeating 5 with 6 behind it: only 6 is new. One that starts past the
        // next number waits for the stream, so 8 is not acted on before 7.
        conn.send_datagram(run(5, &[key(1, 0x62), key(0, 0x62)]))
            .unwrap();
        assert_eq!(next("message 6"), (0x62, false));
        conn.send_datagram(run(8, &[key(0, 0x63)])).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(no_key(), "8 before 7");
        for msg in [key(0, 0x62), key(1, 0x63), key(0, 0x63)] {
            send.write_all(&frame(KIND_BINARY, &msg)).await.unwrap();
        }
        assert_eq!(next("message 7"), (0x63, true));
        assert_eq!(next("message 8"), (0x63, false));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(no_key(), "nothing twice");
        server.abort();
    }

    #[tokio::test]
    async fn a_wrong_token_gets_the_error_and_the_close_code() {
        let (_state, advert, server) = start(&["--token", "secret"]).await;
        let conn = connect(&advert, &stream_path(&advert)).await.unwrap();
        let (_send, mut recv) = hello(&conn, "nope").await;
        let error = read_json(&mut recv).await;
        assert_eq!(error["code"], "auth");
        let (kind, close) = read_frame(&mut recv).await;
        assert_eq!(kind, KIND_CLOSE);
        assert_eq!(
            u16::from_le_bytes([close[0], close[1]]),
            protocol::CLOSE_AUTH
        );
        server.abort();
    }

    #[tokio::test]
    async fn sessions_need_the_ticket() {
        for args in [&["--no-auth"][..], &["--token", "secret"][..]] {
            let (_state, advert, server) = start(args).await;
            assert!(connect(&advert, "/stream").await.is_err(), "no ticket");
            assert!(connect(&advert, "/stream?ticket=wrong").await.is_err());
            assert!(
                connect(&advert, "/elsewhere").await.is_err(),
                "unknown path"
            );
            assert!(connect(&advert, &stream_path(&advert)).await.is_ok());
            server.abort();
        }
        let (_state, advert, server) = start(&["--no-auth"]).await;
        let conn = connect(&advert, &stream_path(&advert)).await.unwrap();
        let (_send, mut recv) = hello(&conn, "").await;
        assert_eq!(read_json(&mut recv).await["t"], "welcome");
        server.abort();
    }
}
