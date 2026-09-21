//! SIP-over-WebSocket transport bridge (RFC 7118).
//!
//! sofia-sip 1.12.11 — the version shipped by Debian/Ubuntu and the one this
//! project links against — has no WebSocket transport: `tport` only implements
//! UDP, TCP and TLS.  Rather than vendor the FreeSWITCH sofia fork, this module
//! terminates the WebSocket leg in Rust and presents sofia a plain TCP socket.
//!
//! ```text
//!   sofia (TCP) ──► 127.0.0.1:<bridge port> ──► [WsBridge] ──► ws(s)://host/path
//! ```
//!
//! The bridge is sofia's *outbound proxy*: `glue.c` points `NUTAG_PROXY` at the
//! loopback port with `;lr`, so every outgoing request carries a `Route` header
//! naming the bridge.  Acting as a loose-routing next hop, the bridge strips
//! that `Route` — exactly what a real proxy does with a route pointing at
//! itself — and rewrites the transport-dependent parts of the message:
//!
//! | direction | rewrite |
//! |-----------|---------|
//! | outbound  | `SIP/2.0/TCP` → `SIP/2.0/WS`, `127.0.0.1:<port>` → `<token>.invalid`, `;transport=tcp` → `;transport=ws` |
//! | inbound   | the exact inverse |
//!
//! The `<token>.invalid` host is what RFC 7118 §5 requires of a WebSocket
//! client: it has no routable address, so the server must return in-dialog
//! requests over the established connection rather than opening a new one.
//!
//! Rewrites are confined to the header block — the SDP body carries the real
//! LAN address for media and must never be touched.
//!
//! Everything runs on the GLib main loop via `gio` async I/O, so the bridge
//! obeys the same single-threaded rule as the rest of the SIP layer.

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::Read;
use std::rc::Rc;

/// GUID from RFC 6455 §1.3, concatenated with the client nonce to derive the
/// expected `Sec-WebSocket-Accept` value.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Cap on a single inbound WebSocket message.  A SIP message is a few KiB at
/// most; anything larger is a malformed or hostile peer and closes the session.
const MAX_WS_MESSAGE: usize = 1 << 20;

/// Cap on buffered-but-unparsed bytes from sofia.  Same reasoning.
const MAX_SIP_BUFFER: usize = 1 << 20;

// ── Configuration ────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct WsConfig {
    /// WebSocket server host (the SIP server).
    pub host: String,
    /// WebSocket server port (typically 443 for wss, 80 for ws).
    pub port: u16,
    /// HTTP resource path of the WebSocket endpoint, e.g. `/ws`.
    pub path: String,
    /// `true` for `wss://` (TLS), `false` for plain `ws://`.
    pub secure: bool,
    /// Verify the server certificate.  Ignored when `secure` is false.
    pub tls_verify: bool,
    /// PEM CA bundle overriding the system trust store.  Empty = system store.
    pub tls_ca_file: String,
}

// ── Bridge handle ────────────────────────────────────────────────────────────

/// A running bridge.  Dropping it cancels all in-flight I/O and closes the
/// listener, so a `SipEngine` teardown tears the WebSocket leg down with it.
pub struct WsBridge {
    port: u16,
    cancel: gio::Cancellable,
}

impl WsBridge {
    /// Bind a loopback listener and start accepting sofia's TCP connections.
    ///
    /// Returns immediately; the outbound WebSocket connection is established
    /// lazily, when sofia first connects.
    pub fn start(cfg: WsConfig) -> Result<Self, glib::Error> {
        let socket = gio::Socket::new(
            gio::SocketFamily::Ipv4,
            gio::SocketType::Stream,
            gio::SocketProtocol::Tcp,
        )?;
        let loopback = gio::InetAddress::from_bytes(gio::InetAddressBytes::V4(&[127, 0, 0, 1]));
        let addr = gio::InetSocketAddress::new(&loopback, 0);
        socket.bind(&addr, true)?;
        socket.listen()?;

        // Read the kernel-assigned port back before handing the socket to the
        // listener — this is the address glue.c dials, so it must be exact.
        let port = socket
            .local_address()?
            .downcast::<gio::InetSocketAddress>()
            .map(|a| a.port())
            .map_err(|_| {
                glib::Error::new(gio::IOErrorEnum::Failed, "bridge socket has no inet address")
            })?;

        let listener = gio::SocketListener::new();
        listener.add_socket(&socket, None::<&glib::Object>)?;

        let cancel = gio::Cancellable::new();
        glib::MainContext::default().spawn_local(accept_loop(
            listener,
            cfg,
            port,
            cancel.clone(),
        ));

        log::info!("ws bridge listening on 127.0.0.1:{port}");
        Ok(WsBridge { port, cancel })
    }

    /// Loopback TCP port that sofia must use as its next hop.
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for WsBridge {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl std::fmt::Debug for WsBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsBridge").field("port", &self.port).finish()
    }
}

// ── Per-session shared state ─────────────────────────────────────────────────

struct Session {
    /// `<random>.invalid` — the host this client advertises to the server.
    token: String,
    /// Loopback address of the bridge itself, used to recognise and strip the
    /// `Route` header sofia adds because the bridge is its configured proxy.
    bridge_addr: String,
    /// sofia's own `sent-by` (host:port), learned from the first outbound Via.
    /// Inbound messages map `token` back to this so sofia matches its own
    /// transactions and accepts in-dialog requests aimed at its Contact.
    sent_by: RefCell<Option<String>>,
}

// ── Accept loop ──────────────────────────────────────────────────────────────

async fn accept_loop(
    listener: gio::SocketListener,
    cfg: WsConfig,
    bridge_port: u16,
    cancel: gio::Cancellable,
) {
    loop {
        let conn = match listener.accept_future().await {
            Ok((conn, _)) => conn,
            Err(e) => {
                if !cancel.is_cancelled() {
                    log::error!("ws bridge: accept failed: {e}");
                }
                return;
            }
        };
        if cancel.is_cancelled() {
            return;
        }
        glib::MainContext::default().spawn_local(run_session(
            conn,
            cfg.clone(),
            bridge_port,
            cancel.clone(),
        ));
    }
}

async fn run_session(
    local: gio::SocketConnection,
    cfg: WsConfig,
    bridge_port: u16,
    cancel: gio::Cancellable,
) {
    let session = Rc::new(Session {
        token: format!("{:08x}{:08x}.invalid", glib::random_int(), glib::random_int()),
        bridge_addr: format!("127.0.0.1:{bridge_port}"),
        sent_by: RefCell::new(None),
    });

    let remote = match connect_ws(&cfg, &cancel).await {
        Ok(c) => c,
        Err(e) => {
            log::error!(
                "ws bridge: connection to {}://{}:{}{} failed: {e}",
                if cfg.secure { "wss" } else { "ws" },
                cfg.host,
                cfg.port,
                cfg.path
            );
            let _ = local.close_future(glib::Priority::DEFAULT).await;
            return;
        }
    };

    log::info!(
        "ws bridge: connected to {}://{}:{}{} as {}",
        if cfg.secure { "wss" } else { "ws" },
        cfg.host,
        cfg.port,
        cfg.path,
        session.token
    );

    // Serialised writers: the ws side is written by both pumps (SIP messages
    // from sofia, and pong/close frames from the reader), so its writes must
    // not interleave mid-frame.
    let ws_writer = Writer::new(remote.conn.output_stream());
    let tcp_writer = Writer::new(local.output_stream());

    let done = Rc::new(RefCell::new(false));

    glib::MainContext::default().spawn_local(pump_sofia_to_ws(
        local.clone(),
        ws_writer.clone(),
        session.clone(),
        done.clone(),
        cancel.clone(),
    ));

    pump_ws_to_sofia(
        remote,
        ws_writer,
        tcp_writer,
        session,
        done,
        cancel,
    )
    .await;

    let _ = local.close_future(glib::Priority::DEFAULT).await;
}

// ── WebSocket connection + handshake ─────────────────────────────────────────

struct WsConn {
    conn: gio::IOStream,
    /// Bytes read past the end of the HTTP handshake response — the first
    /// WebSocket frames may already be in this buffer and must not be lost.
    leftover: Vec<u8>,
}

async fn connect_ws(cfg: &WsConfig, cancel: &gio::Cancellable) -> Result<WsConn, glib::Error> {
    let client = gio::SocketClient::new();
    if cfg.secure {
        client.set_tls(true);

        if !cfg.tls_verify {
            // Accept any certificate.  Mirrors the tls_verify=0 behaviour of
            // the sofia TLS transport (TPTAG_TLS_VERIFY_PEER(0)).
            client.connect_event(|_, event, _, conn| {
                if event == gio::SocketClientEvent::TlsHandshaking {
                    if let Some(tls) = conn.and_then(|c| c.clone().downcast::<gio::TlsClientConnection>().ok())
                    {
                        tls.connect_accept_certificate(|_, _, _| true);
                    }
                }
            });
        } else if !cfg.tls_ca_file.is_empty() {
            // A custom CA bundle replaces the system trust store for this
            // connection, matching how glue.c treats tls_ca_file.
            let db = gio::TlsFileDatabase::new(cfg.tls_ca_file.as_str())?;
            client.connect_event(move |_, event, _, conn| {
                if event == gio::SocketClientEvent::TlsHandshaking {
                    if let Some(tls) = conn.and_then(|c| c.clone().downcast::<gio::TlsClientConnection>().ok())
                    {
                        tls.set_database(Some(&db));
                    }
                }
            });
        }
    }

    let conn = client
        .connect_to_host_future(&format!("{}:{}", cfg.host, cfg.port), cfg.port)
        .await?;
    if cancel.is_cancelled() {
        return Err(glib::Error::new(gio::IOErrorEnum::Cancelled, "bridge cancelled"));
    }
    let stream: gio::IOStream = conn.upcast();

    // ── Client handshake (RFC 6455 §4.1) ──────────────────────────────────
    let nonce = random_bytes(16);
    let key = glib::base64_encode(&nonce).to_string();
    let host_hdr = if (cfg.secure && cfg.port == 443) || (!cfg.secure && cfg.port == 80) {
        cfg.host.clone()
    } else {
        format!("{}:{}", cfg.host, cfg.port)
    };
    let request = format!(
        "GET {} HTTP/1.1\r\n\
         Host: {}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {}\r\n\
         Sec-WebSocket-Protocol: sip\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n",
        if cfg.path.is_empty() { "/" } else { &cfg.path },
        host_hdr,
        key
    );
    write_all(&stream.output_stream(), request.into_bytes()).await?;

    // Read until the end of the HTTP response headers.
    let mut buf: Vec<u8> = Vec::new();
    let head_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 16 * 1024 {
            return Err(glib::Error::new(
                gio::IOErrorEnum::Failed,
                "websocket handshake response too large",
            ));
        }
        let chunk = read_some(&stream.input_stream()).await?;
        if chunk.is_empty() {
            return Err(glib::Error::new(
                gio::IOErrorEnum::Failed,
                "server closed during websocket handshake",
            ));
        }
        buf.extend_from_slice(&chunk);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let status_ok = head
        .lines()
        .next()
        .map(|l| l.contains(" 101"))
        .unwrap_or(false);
    if !status_ok {
        let first = head.lines().next().unwrap_or("").to_string();
        return Err(glib::Error::new(
            gio::IOErrorEnum::Failed,
            &format!("websocket upgrade rejected: {first}"),
        ));
    }

    // Verify Sec-WebSocket-Accept so a plain HTTP endpoint that happens to
    // answer 101 cannot be mistaken for a WebSocket server.
    let expect = accept_key(&key);
    let got = header_value(&head, "sec-websocket-accept").unwrap_or_default();
    if got != expect {
        return Err(glib::Error::new(
            gio::IOErrorEnum::Failed,
            "websocket handshake: Sec-WebSocket-Accept mismatch",
        ));
    }

    Ok(WsConn { conn: stream, leftover: buf[head_end..].to_vec() })
}

/// Derive the `Sec-WebSocket-Accept` value a conforming server must return for
/// `key`: base64(SHA-1(key ++ GUID)), per RFC 6455 §4.2.2.
fn accept_key(key: &str) -> String {
    let mut cs =
        glib::Checksum::new(glib::ChecksumType::Sha1).expect("SHA-1 is always available in GLib");
    cs.update(format!("{key}{WS_GUID}").as_bytes());
    glib::base64_encode(&cs.digest()).to_string()
}

/// Case-insensitive lookup of an HTTP header value in a raw header block.
fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim().eq_ignore_ascii_case(name)).then(|| v.trim().to_string())
    })
}

// ── Pump: sofia → WebSocket ──────────────────────────────────────────────────

async fn pump_sofia_to_ws(
    local: gio::SocketConnection,
    ws: Writer,
    session: Rc<Session>,
    done: Rc<RefCell<bool>>,
    cancel: gio::Cancellable,
) {
    let input = local.input_stream();
    let mut buf: Vec<u8> = Vec::new();

    loop {
        if *done.borrow() || cancel.is_cancelled() {
            break;
        }
        let chunk = match read_some(&input).await {
            Ok(c) if c.is_empty() => break, // sofia closed the socket
            Ok(c) => c,
            Err(e) => {
                if !cancel.is_cancelled() {
                    log::debug!("ws bridge: local read ended: {e}");
                }
                break;
            }
        };
        buf.extend_from_slice(&chunk);
        if buf.len() > MAX_SIP_BUFFER {
            log::error!("ws bridge: oversized SIP message from sofia, dropping session");
            break;
        }

        while let Some(msg) = take_sip_message(&mut buf) {
            let text = String::from_utf8_lossy(&msg).to_string();
            let out = rewrite_outbound(&text, &session);
            log::trace!("ws bridge ► {}", first_line(&out));
            ws.push(encode_frame(OP_TEXT, out.as_bytes()));
        }
    }

    *done.borrow_mut() = true;
    ws.push(encode_frame(OP_CLOSE, &1000u16.to_be_bytes()));
}

// ── Pump: WebSocket → sofia ──────────────────────────────────────────────────

async fn pump_ws_to_sofia(
    remote: WsConn,
    ws: Writer,
    tcp: Writer,
    session: Rc<Session>,
    done: Rc<RefCell<bool>>,
    cancel: gio::Cancellable,
) {
    let input = remote.conn.input_stream();
    let mut buf = remote.leftover;
    // Reassembly state for fragmented messages (opcode of the first frame,
    // plus the payload accumulated so far).
    let mut frag: Option<(u8, Vec<u8>)> = None;

    loop {
        // Drain every complete frame already buffered before reading more.
        loop {
            match decode_frame(&buf) {
                Ok(None) => break,
                Err(e) => {
                    log::error!("ws bridge: {e}");
                    *done.borrow_mut() = true;
                    return;
                }
                Ok(Some(frame)) => {
                    buf.drain(..frame.consumed);
                    match frame.opcode {
                        OP_CLOSE => {
                            log::info!("ws bridge: server closed the websocket");
                            *done.borrow_mut() = true;
                            ws.push(encode_frame(OP_CLOSE, &1000u16.to_be_bytes()));
                            return;
                        }
                        OP_PING => {
                            ws.push(encode_frame(OP_PONG, &frame.payload));
                            continue;
                        }
                        OP_PONG => continue,
                        _ => {}
                    }

                    // Data frame: handle fragmentation (opcode 0 = continuation).
                    let complete = match (&mut frag, frame.opcode) {
                        (slot @ None, op) if op == OP_TEXT || op == OP_BINARY => {
                            if frame.fin {
                                Some(frame.payload)
                            } else {
                                *slot = Some((op, frame.payload));
                                None
                            }
                        }
                        (Some((_, acc)), OP_CONT) => {
                            acc.extend_from_slice(&frame.payload);
                            if acc.len() > MAX_WS_MESSAGE {
                                log::error!("ws bridge: fragmented message exceeds limit");
                                *done.borrow_mut() = true;
                                return;
                            }
                            if frame.fin {
                                frag.take().map(|(_, acc)| acc)
                            } else {
                                None
                            }
                        }
                        _ => {
                            log::error!("ws bridge: unexpected frame sequence, closing");
                            *done.borrow_mut() = true;
                            return;
                        }
                    };

                    if let Some(payload) = complete {
                        let text = String::from_utf8_lossy(&payload).to_string();
                        // RFC 7118 §5: a lone CRLF is a keep-alive ping, not a
                        // SIP message — sofia has its own keep-alive handling
                        // and would only log a parse error, so drop it here.
                        if text.trim().is_empty() {
                            continue;
                        }
                        let out = rewrite_inbound(&text, &session);
                        log::trace!("ws bridge ◄ {}", first_line(&out));
                        tcp.push(out.into_bytes());
                    }
                }
            }
        }

        if *done.borrow() || cancel.is_cancelled() {
            return;
        }
        match read_some(&input).await {
            Ok(c) if c.is_empty() => {
                log::info!("ws bridge: websocket closed by peer");
                *done.borrow_mut() = true;
                return;
            }
            Ok(c) => {
                buf.extend_from_slice(&c);
                if buf.len() > MAX_WS_MESSAGE {
                    log::error!("ws bridge: oversized websocket frame, dropping session");
                    *done.borrow_mut() = true;
                    return;
                }
            }
            Err(e) => {
                if !cancel.is_cancelled() {
                    log::debug!("ws bridge: websocket read ended: {e}");
                }
                *done.borrow_mut() = true;
                return;
            }
        }
    }
}

// ── Serialised async writer ──────────────────────────────────────────────────

/// Queues writes so two producers can share one `OutputStream` without
/// interleaving bytes mid-frame.
#[derive(Clone)]
struct Writer {
    stream: gio::OutputStream,
    queue: Rc<RefCell<VecDeque<Vec<u8>>>>,
    busy: Rc<RefCell<bool>>,
}

impl Writer {
    fn new(stream: impl IsA<gio::OutputStream>) -> Self {
        Writer {
            stream: stream.upcast(),
            queue: Rc::new(RefCell::new(VecDeque::new())),
            busy: Rc::new(RefCell::new(false)),
        }
    }

    fn push(&self, data: Vec<u8>) {
        self.queue.borrow_mut().push_back(data);
        if *self.busy.borrow() {
            return;
        }
        *self.busy.borrow_mut() = true;
        let this = self.clone();
        glib::MainContext::default().spawn_local(async move {
            loop {
                let next = this.queue.borrow_mut().pop_front();
                let Some(data) = next else { break };
                if write_all(&this.stream, data).await.is_err() {
                    this.queue.borrow_mut().clear();
                    break;
                }
            }
            *this.busy.borrow_mut() = false;
        });
    }
}

// ── Async I/O helpers ────────────────────────────────────────────────────────

async fn write_all(stream: &gio::OutputStream, data: Vec<u8>) -> Result<(), glib::Error> {
    match stream.write_all_future(data, glib::Priority::DEFAULT).await {
        Ok((_, _, Some(e))) => Err(e),
        Ok(_) => Ok(()),
        Err((_, e)) => Err(e),
    }
}

async fn read_some(stream: &gio::InputStream) -> Result<Vec<u8>, glib::Error> {
    let buf = vec![0u8; 8192];
    match stream.read_future(buf, glib::Priority::DEFAULT).await {
        Ok((mut buf, n)) => {
            buf.truncate(n);
            Ok(buf)
        }
        Err((_, e)) => Err(e),
    }
}

/// Cryptographically unpredictable bytes for the handshake nonce and the
/// per-frame masking keys.  RFC 6455 §5.3 requires masking keys the peer
/// cannot guess; `glib::random_int()` is a deterministic PRNG, so read from
/// the OS instead and only fall back to GLib if that fails.
fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut out).is_ok() {
            return out;
        }
    }
    for chunk in out.chunks_mut(4) {
        let r = glib::random_int().to_ne_bytes();
        chunk.copy_from_slice(&r[..chunk.len()]);
    }
    out
}

fn first_line(msg: &str) -> &str {
    msg.lines().next().unwrap_or("")
}

// ── WebSocket framing (RFC 6455 §5) ──────────────────────────────────────────

const OP_CONT: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
    /// Bytes of the input buffer this frame occupied.
    consumed: usize,
}

/// Encode a single unfragmented client frame.  Client-to-server frames MUST be
/// masked (RFC 6455 §5.3).
fn encode_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | opcode); // FIN set
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | len as u8);
    } else if len <= u16::MAX as usize {
        out.push(0x80 | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    let mask = random_bytes(4);
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

/// Decode one frame from the front of `buf`.
///
/// `Ok(None)` means the frame is incomplete and more bytes are needed.
fn decode_frame(buf: &[u8]) -> Result<Option<Frame>, String> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let fin = buf[0] & 0x80 != 0;
    if buf[0] & 0x70 != 0 {
        return Err("reserved websocket bits set (no extension negotiated)".into());
    }
    let opcode = buf[0] & 0x0F;
    let masked = buf[1] & 0x80 != 0;
    let len7 = (buf[1] & 0x7F) as usize;

    let mut off = 2;
    let len = match len7 {
        126 => {
            if buf.len() < off + 2 {
                return Ok(None);
            }
            let l = u16::from_be_bytes([buf[off], buf[off + 1]]) as usize;
            off += 2;
            l
        }
        127 => {
            if buf.len() < off + 8 {
                return Ok(None);
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[off..off + 8]);
            off += 8;
            u64::from_be_bytes(b) as usize
        }
        n => n,
    };

    if len > MAX_WS_MESSAGE {
        return Err(format!("websocket frame of {len} bytes exceeds limit"));
    }
    // A server MUST NOT mask (RFC 6455 §5.1); tolerate it rather than fail, but
    // the mask must be present in the buffer before the payload can be read.
    let mask = if masked {
        if buf.len() < off + 4 {
            return Ok(None);
        }
        let m = [buf[off], buf[off + 1], buf[off + 2], buf[off + 3]];
        off += 4;
        Some(m)
    } else {
        None
    };

    if buf.len() < off + len {
        return Ok(None);
    }
    let mut payload = buf[off..off + len].to_vec();
    if let Some(m) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= m[i % 4];
        }
    }
    Ok(Some(Frame { fin, opcode, payload, consumed: off + len }))
}

// ── SIP message framing on the TCP side ──────────────────────────────────────

/// Split one complete SIP message off the front of `buf`, honouring
/// `Content-Length` (RFC 3261 §7.5 framing over a stream transport).
///
/// Leading CRLFs — sofia's stream keep-alives — are consumed and discarded.
fn take_sip_message(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    // Discard any leading CR/LF padding before the start line.
    let start = buf.iter().position(|b| *b != b'\r' && *b != b'\n')?;
    if start > 0 {
        buf.drain(..start);
    }

    let head_end = find(buf, b"\r\n\r\n")? + 4;
    let head = String::from_utf8_lossy(&buf[..head_end]);
    let body_len = content_length(&head).unwrap_or(0);
    let total = head_end + body_len;
    if buf.len() < total {
        return None;
    }
    Some(buf.drain(..total).collect())
}

/// Read `Content-Length` (or its compact form `l`) from a header block.
fn content_length(head: &str) -> Option<usize> {
    head.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        let k = k.trim();
        (k.eq_ignore_ascii_case("Content-Length") || k.eq_ignore_ascii_case("l"))
            .then(|| v.trim().parse().ok())
            .flatten()
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ── Header rewriting ─────────────────────────────────────────────────────────

/// Split a SIP message into its header block (including the blank-line
/// terminator) and its body.  Only the header block is ever rewritten — the SDP
/// body carries the real media address and must pass through untouched.
fn split_head(msg: &str) -> (&str, &str) {
    match msg.find("\r\n\r\n") {
        Some(p) => msg.split_at(p + 4),
        None => (msg, ""),
    }
}

/// sofia → server.
fn rewrite_outbound(msg: &str, session: &Session) -> String {
    let (head, body) = split_head(msg);

    let mut out = String::with_capacity(msg.len() + 64);
    for line in head.split_inclusive("\r\n") {
        let trimmed = line.trim_end_matches(['\r', '\n']);

        // Learn sofia's sent-by from the topmost Via so inbound messages can be
        // mapped back to an address sofia recognises as its own.
        if session.sent_by.borrow().is_none() {
            if let Some(sb) = via_sent_by(trimmed) {
                *session.sent_by.borrow_mut() = Some(sb);
            }
        }

        // The bridge is sofia's loose-routing next hop, so a Route naming the
        // bridge is consumed here exactly as a proxy would consume it.
        if is_header(trimmed, "Route", "r") && trimmed.contains(&session.bridge_addr) {
            continue;
        }

        out.push_str(&map_tokens(line, &session.token, true));
    }
    out.push_str(body);
    out
}

/// server → sofia.
fn rewrite_inbound(msg: &str, session: &Session) -> String {
    let (head, body) = split_head(msg);
    let sent_by = session.sent_by.borrow().clone();

    let mut out = String::with_capacity(msg.len() + 64);
    for line in head.split_inclusive("\r\n") {
        let mut line = map_tokens(line, &session.token, false);
        // Restore sofia's own address wherever the server echoed our token.
        if let Some(sb) = &sent_by {
            line = line.replace(&session.token, sb);
        }
        out.push_str(&line);
    }
    out.push_str(body);
    out
}

/// Rewrite the transport-dependent tokens of one header line.
///
/// `outbound == true` maps TCP → WS and the loopback address → the `.invalid`
/// token; `false` performs the inverse (except the token → sent-by mapping,
/// which the caller applies because it needs the learned address).
fn map_tokens(line: &str, token: &str, outbound: bool) -> String {
    if outbound {
        let line = line.replace("SIP/2.0/TCP", "SIP/2.0/WS");
        let line = replace_loopback(&line, token);
        replace_ci(&line, ";transport=tcp", ";transport=ws")
    } else {
        let line = line.replace("SIP/2.0/WSS", "SIP/2.0/TCP");
        let line = line.replace("SIP/2.0/WS", "SIP/2.0/TCP");
        // Longest first, so "wss" is not left as a stray "s".
        let line = replace_ci(&line, ";transport=wss", ";transport=tcp");
        replace_ci(&line, ";transport=ws", ";transport=tcp")
    }
}

/// Replace every `127.0.0.1:<port>` occurrence with `token`.
///
/// Matching the port too (rather than the bare address) keeps the substitution
/// anchored to a full host:port, so the result is a syntactically valid SIP
/// host with no dangling `:5060` behind the `.invalid` name.
fn replace_loopback(line: &str, token: &str) -> String {
    const LOOPBACK: &str = "127.0.0.1:";
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(pos) = rest.find(LOOPBACK) {
        let after = &rest[pos + LOOPBACK.len()..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        out.push_str(&rest[..pos]);
        if digits == 0 {
            // Not a host:port after all — copy the literal through unchanged.
            out.push_str(LOOPBACK);
        } else {
            out.push_str(token);
        }
        rest = &after[digits..];
    }
    out.push_str(rest);
    out
}

/// Case-insensitive `str::replace`.  SIP parameter names are case-insensitive,
/// so `;TRANSPORT=TCP` must be rewritten just like `;transport=tcp`.
fn replace_ci(haystack: &str, needle: &str, replacement: &str) -> String {
    let lower = haystack.to_ascii_lowercase();
    let needle = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0;
    while let Some(pos) = lower[i..].find(&needle) {
        let at = i + pos;
        out.push_str(&haystack[i..at]);
        out.push_str(replacement);
        i = at + needle.len();
    }
    out.push_str(&haystack[i..]);
    out
}

/// True when `line` is the named header, in either long or compact form.
fn is_header(line: &str, long: &str, compact: &str) -> bool {
    match line.split_once(':') {
        Some((name, _)) => {
            let name = name.trim();
            name.eq_ignore_ascii_case(long) || name.eq_ignore_ascii_case(compact)
        }
        None => false,
    }
}

/// Extract the `sent-by` (host:port) from a Via header line, if it is one.
fn via_sent_by(line: &str) -> Option<String> {
    if !is_header(line, "Via", "v") {
        return None;
    }
    let (_, value) = line.split_once(':')?;
    // "SIP/2.0/TCP 127.0.0.1:5080;branch=z9hG4bK..."
    let mut parts = value.trim().splitn(2, ' ');
    let proto = parts.next()?;
    if !proto.starts_with("SIP/2.0/") {
        return None;
    }
    let rest = parts.next()?.trim();
    let end = rest.find([';', ',', ' ']).unwrap_or(rest.len());
    let sent_by = rest[..end].trim();
    (!sent_by.is_empty()).then(|| sent_by.to_string())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session {
            token: "deadbeef.invalid".into(),
            bridge_addr: "127.0.0.1:9999".into(),
            sent_by: RefCell::new(None),
        }
    }

    // ── SIP stream framing ────────────────────────────────────────────────

    #[test]
    fn takes_a_message_with_a_body() {
        let msg = "INVITE sip:1@x SIP/2.0\r\nContent-Length: 5\r\n\r\nhello";
        let mut buf = msg.as_bytes().to_vec();
        let got = take_sip_message(&mut buf).unwrap();
        assert_eq!(got, msg.as_bytes());
        assert!(buf.is_empty());
    }

    #[test]
    fn waits_for_an_incomplete_body() {
        let raw = b"INVITE sip:1@x SIP/2.0\r\nContent-Length: 10\r\n\r\nshort";
        let mut buf = raw.to_vec();
        assert!(take_sip_message(&mut buf).is_none());
        // Nothing consumed: the caller must be able to append and retry.
        assert_eq!(buf, raw);
    }

    #[test]
    fn splits_two_pipelined_messages() {
        let mut buf =
            b"OPTIONS sip:x SIP/2.0\r\nContent-Length: 0\r\n\r\nBYE sip:x SIP/2.0\r\nContent-Length: 0\r\n\r\n"
                .to_vec();
        let a = take_sip_message(&mut buf).unwrap();
        assert!(a.starts_with(b"OPTIONS"));
        let b = take_sip_message(&mut buf).unwrap();
        assert!(b.starts_with(b"BYE"));
        assert!(take_sip_message(&mut buf).is_none());
    }

    #[test]
    fn discards_leading_crlf_keepalive() {
        let mut buf = b"\r\n\r\nBYE sip:x SIP/2.0\r\nContent-Length: 0\r\n\r\n".to_vec();
        let m = take_sip_message(&mut buf).unwrap();
        assert!(m.starts_with(b"BYE"));
    }

    #[test]
    fn reads_compact_content_length() {
        assert_eq!(content_length("l: 7\r\n"), Some(7));
        assert_eq!(content_length("Content-Length: 12\r\n"), Some(12));
        assert_eq!(content_length("Subject: none\r\n"), None);
    }

    // ── Header rewriting ──────────────────────────────────────────────────

    #[test]
    fn outbound_rewrites_via_and_contact_and_strips_route() {
        let s = session();
        let msg = "INVITE sip:887@pbx.example.com SIP/2.0\r\n\
                   Via: SIP/2.0/TCP 127.0.0.1:5080;branch=z9hG4bK1\r\n\
                   Route: <sip:127.0.0.1:9999;transport=tcp;lr>\r\n\
                   Contact: <sip:bob@127.0.0.1:5080;transport=tcp>\r\n\
                   Content-Length: 0\r\n\r\n";
        let out = rewrite_outbound(msg, &s);
        assert!(out.contains("Via: SIP/2.0/WS deadbeef.invalid;branch=z9hG4bK1\r\n"));
        assert!(out.contains("Contact: <sip:bob@deadbeef.invalid;transport=ws>\r\n"));
        assert!(!out.contains("Route:"), "route to the bridge must be consumed");
        assert_eq!(*s.sent_by.borrow(), Some("127.0.0.1:5080".to_string()));
    }

    #[test]
    fn outbound_keeps_a_route_that_is_not_the_bridge() {
        let s = session();
        let msg = "INVITE sip:1@x SIP/2.0\r\n\
                   Route: <sip:edge.example.com;lr>\r\n\
                   Content-Length: 0\r\n\r\n";
        assert!(rewrite_outbound(msg, &s).contains("Route: <sip:edge.example.com;lr>"));
    }

    #[test]
    fn inbound_restores_the_learned_sent_by() {
        let s = session();
        *s.sent_by.borrow_mut() = Some("127.0.0.1:5080".into());
        let msg = "SIP/2.0 200 OK\r\n\
                   Via: SIP/2.0/WS deadbeef.invalid;branch=z9hG4bK1\r\n\
                   Contact: <sip:887@pbx.example.com;transport=ws>\r\n\
                   Content-Length: 0\r\n\r\n";
        let out = rewrite_inbound(msg, &s);
        assert!(out.contains("Via: SIP/2.0/TCP 127.0.0.1:5080;branch=z9hG4bK1\r\n"));
        assert!(out.contains("Contact: <sip:887@pbx.example.com;transport=tcp>\r\n"));
    }

    #[test]
    fn round_trip_preserves_the_original_headers() {
        let s = session();
        let msg = "INVITE sip:887@pbx SIP/2.0\r\n\
                   Via: SIP/2.0/TCP 127.0.0.1:5080;branch=z9hG4bK1\r\n\
                   Contact: <sip:bob@127.0.0.1:5080;transport=tcp>\r\n\
                   Content-Length: 0\r\n\r\n";
        let wire = rewrite_outbound(msg, &s);
        assert_eq!(rewrite_inbound(&wire, &s), msg);
    }

    #[test]
    fn the_sdp_body_is_never_rewritten() {
        let s = session();
        // A body that contains every token the header rewriter looks for.
        let body = "v=0\r\no=- 1 1 IN IP4 127.0.0.1:5080\r\nc=IN IP4 192.168.1.5\r\na=x;transport=tcp\r\n";
        let msg = format!(
            "INVITE sip:1@x SIP/2.0\r\nVia: SIP/2.0/TCP 127.0.0.1:5080\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let out = rewrite_outbound(&msg, &s);
        assert!(out.ends_with(body), "media address must survive untouched");
    }

    #[test]
    fn wss_transport_param_maps_back_to_tcp() {
        let s = session();
        let msg = "SIP/2.0 200 OK\r\nContact: <sip:a@b;transport=wss>\r\n\r\n";
        assert!(rewrite_inbound(msg, &s).contains(";transport=tcp>"));
    }

    #[test]
    fn transport_param_rewrite_is_case_insensitive() {
        assert_eq!(replace_ci("a;TRANSPORT=TCP;b", ";transport=tcp", ";transport=ws"), "a;transport=ws;b");
    }

    #[test]
    fn loopback_without_a_port_is_left_alone() {
        // A bare address is not a host:port and must not become "<token>".
        assert_eq!(replace_loopback("sip:a@127.0.0.1;lr", "t.invalid"), "sip:a@127.0.0.1;lr");
        assert_eq!(replace_loopback("sip:a@127.0.0.1:506;lr", "t.invalid"), "sip:a@t.invalid;lr");
    }

    #[test]
    fn via_sent_by_parsing() {
        assert_eq!(
            via_sent_by("Via: SIP/2.0/TCP 10.0.0.1:5060;branch=z9hG4bK1"),
            Some("10.0.0.1:5060".into())
        );
        assert_eq!(via_sent_by("v: SIP/2.0/WS host.invalid"), Some("host.invalid".into()));
        assert_eq!(via_sent_by("Contact: <sip:a@b>"), None);
    }

    // ── WebSocket framing ─────────────────────────────────────────────────

    #[test]
    fn frames_round_trip_through_the_decoder() {
        for len in [0usize, 5, 125, 126, 200, 70_000] {
            let payload = vec![b'x'; len];
            let encoded = encode_frame(OP_TEXT, &payload);
            let f = decode_frame(&encoded).unwrap().unwrap();
            assert!(f.fin);
            assert_eq!(f.opcode, OP_TEXT);
            assert_eq!(f.payload, payload, "payload mismatch at len {len}");
            assert_eq!(f.consumed, encoded.len());
        }
    }

    #[test]
    fn client_frames_are_masked() {
        let encoded = encode_frame(OP_TEXT, b"REGISTER");
        assert_eq!(encoded[1] & 0x80, 0x80, "RFC 6455 requires client masking");
        // The plaintext must not appear on the wire.
        assert!(find(&encoded, b"REGISTER").is_none());
    }

    #[test]
    fn a_partial_frame_asks_for_more_bytes() {
        let encoded = encode_frame(OP_TEXT, &vec![b'y'; 300]);
        for cut in [1usize, 2, 3, 4, 8, 100] {
            assert!(decode_frame(&encoded[..cut]).unwrap().is_none(), "cut {cut}");
        }
    }

    #[test]
    fn decoder_rejects_reserved_bits() {
        let mut encoded = encode_frame(OP_TEXT, b"hi");
        encoded[0] |= 0x40;
        assert!(decode_frame(&encoded).is_err());
    }

    #[test]
    fn decoder_accepts_a_server_masked_frame() {
        // Servers must not mask, but tolerate it rather than break the call.
        let encoded = encode_frame(OP_BINARY, b"payload");
        let f = decode_frame(&encoded).unwrap().unwrap();
        assert_eq!(f.payload, b"payload");
    }

    // These vectors come from the RFC itself, so they check the implementation
    // against the specification rather than against its own assumptions.

    #[test]
    fn accept_key_matches_the_rfc6455_example() {
        // RFC 6455 §1.3.
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn decodes_the_rfc6455_unmasked_hello() {
        // RFC 6455 §5.7: a single-frame unmasked text message.
        let bytes = [0x81u8, 0x05, 0x48, 0x65, 0x6c, 0x6c, 0x6f];
        let f = decode_frame(&bytes).unwrap().unwrap();
        assert!(f.fin);
        assert_eq!(f.opcode, OP_TEXT);
        assert_eq!(f.payload, b"Hello");
        assert_eq!(f.consumed, bytes.len());
    }

    #[test]
    fn decodes_the_rfc6455_masked_hello() {
        // RFC 6455 §5.7: the same message, masked with 0x37fa213d.
        let bytes = [
            0x81u8, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58,
        ];
        let f = decode_frame(&bytes).unwrap().unwrap();
        assert_eq!(f.payload, b"Hello");
        assert_eq!(f.consumed, bytes.len());
    }

    #[test]
    fn decodes_the_rfc6455_fragmented_hello() {
        // RFC 6455 §5.7: "Hel" (text, FIN clear) then "lo" (continuation, FIN set).
        let first = decode_frame(&[0x01u8, 0x03, 0x48, 0x65, 0x6c]).unwrap().unwrap();
        assert!(!first.fin);
        assert_eq!(first.opcode, OP_TEXT);
        assert_eq!(first.payload, b"Hel");

        let second = decode_frame(&[0x80u8, 0x02, 0x6c, 0x6f]).unwrap().unwrap();
        assert!(second.fin);
        assert_eq!(second.opcode, OP_CONT);
        assert_eq!(second.payload, b"lo");
    }

    #[test]
    fn extended_length_headers_match_the_rfc6455_layout() {
        // RFC 6455 §5.7: 256 bytes use a 16-bit length, 65536 a 64-bit one.
        let medium = encode_frame(OP_BINARY, &vec![0u8; 256]);
        assert_eq!(&medium[..4], &[0x82, 0xFE, 0x01, 0x00]);

        let large = encode_frame(OP_BINARY, &vec![0u8; 65536]);
        assert_eq!(&large[..10], &[0x82, 0xFF, 0, 0, 0, 0, 0, 0x01, 0, 0]);
    }

    #[test]
    fn handshake_header_lookup_is_case_insensitive() {
        let head = "HTTP/1.1 101 Switching Protocols\r\nSec-WebSocket-Accept: abc=\r\n\r\n";
        assert_eq!(header_value(head, "sec-websocket-accept"), Some("abc=".into()));
        assert_eq!(header_value(head, "missing"), None);
    }
}

// ── End-to-end test ──────────────────────────────────────────────────────────

/// Drives a real `WsBridge` against a minimal in-process WebSocket server, so
/// the async plumbing — handshake, masking, both pumps and the header
/// rewriting — is exercised over actual sockets rather than in isolation.
#[cfg(test)]
mod e2e {
    use super::*;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// A canned 200 OK the fake server returns, in the form a WebSocket peer
    /// would send it: `SIP/2.0/WS` Via and a `;transport=ws` Contact.
    fn canned_response(token: &str) -> String {
        format!(
            "SIP/2.0 200 OK\r\n\
             Via: SIP/2.0/WS {token};branch=z9hG4bKtest\r\n\
             Contact: <sip:bob@pbx.example.com;transport=ws>\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }

    /// Minimal RFC 6455 server: completes the handshake, reads one client
    /// frame, reports it, and replies with one unmasked frame.
    fn spawn_fake_server() -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake server");
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();

        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");

            // ── Handshake ─────────────────────────────────────────────────
            let mut buf = Vec::new();
            let head_end = loop {
                let mut chunk = [0u8; 1024];
                let n = sock.read(&mut chunk).expect("read handshake");
                assert!(n > 0, "client closed during handshake");
                buf.extend_from_slice(&chunk[..n]);
                if let Some(p) = find(&buf, b"\r\n\r\n") {
                    break p + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            assert!(head.starts_with("GET /ws HTTP/1.1\r\n"), "request line: {head}");
            assert_eq!(header_value(&head, "upgrade").as_deref(), Some("websocket"));
            assert_eq!(header_value(&head, "sec-websocket-version").as_deref(), Some("13"));
            assert_eq!(header_value(&head, "sec-websocket-protocol").as_deref(), Some("sip"));
            let key = header_value(&head, "sec-websocket-key").expect("client nonce");

            sock.write_all(
                format!(
                    "HTTP/1.1 101 Switching Protocols\r\n\
                     Upgrade: websocket\r\n\
                     Connection: Upgrade\r\n\
                     Sec-WebSocket-Protocol: sip\r\n\
                     Sec-WebSocket-Accept: {}\r\n\r\n",
                    accept_key(&key)
                )
                .as_bytes(),
            )
            .expect("write handshake response");

            // ── One client frame in ───────────────────────────────────────
            let mut buf = buf[head_end..].to_vec();
            let frame = loop {
                if let Some(f) = decode_frame(&buf).expect("decode client frame") {
                    break f;
                }
                let mut chunk = [0u8; 4096];
                let n = sock.read(&mut chunk).expect("read frame");
                assert!(n > 0, "client closed before sending a frame");
                buf.extend_from_slice(&chunk[..n]);
            };
            assert_eq!(frame.opcode, OP_TEXT);
            let received = String::from_utf8(frame.payload).expect("utf-8 SIP message");

            // Address the reply to whatever .invalid host the bridge invented.
            let token = received
                .lines()
                .find_map(via_sent_by)
                .expect("a Via header in the request");
            let reply = canned_response(&token);
            tx.send(received).expect("report to test");

            // ── One server frame out (unmasked, per RFC 6455 §5.1) ────────
            let payload = reply.as_bytes();
            let mut out = vec![0x81u8]; // FIN | text
            // The reply is longer than 125 bytes, so it needs the 16-bit
            // extended length; squeezing it into the 7-bit field would set the
            // mask bit and corrupt the frame.
            assert!(payload.len() > 125 && payload.len() <= u16::MAX as usize);
            out.push(126);
            out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            out.extend_from_slice(payload);
            sock.write_all(&out).expect("write reply frame");
            sock.flush().ok();

            // Hold the connection open until the test finishes with it.
            std::thread::sleep(Duration::from_secs(2));
        });

        (port, rx)
    }

    #[test]
    fn a_sip_message_survives_a_round_trip_through_a_real_websocket() {
        // The bridge schedules its I/O on the default main context, so this
        // test owns that context and pumps it by hand.
        let _ = env_logger::builder().is_test(true).filter_level(log::LevelFilter::Trace).try_init();
        let _lock = crate::test_support::main_context_lock();
        let main = glib::MainContext::default();
        let _guard = main.acquire().expect("default main context is free");

        let (server_port, received) = spawn_fake_server();

        let bridge = WsBridge::start(WsConfig {
            host: "127.0.0.1".into(),
            port: server_port,
            path: "/ws".into(),
            secure: false,
            tls_verify: false,
            tls_ca_file: String::new(),
        })
        .expect("bridge starts");

        // Stand in for sofia: connect to the bridge and speak plain SIP/TCP.
        let mut sofia = TcpStream::connect(("127.0.0.1", bridge.port())).expect("dial bridge");
        sofia.set_nonblocking(true).expect("nonblocking");
        let request = format!(
            "REGISTER sip:pbx.example.com SIP/2.0\r\n\
             Via: SIP/2.0/TCP 127.0.0.1:5080;branch=z9hG4bKtest\r\n\
             Route: <sip:127.0.0.1:{};transport=tcp;lr>\r\n\
             Contact: <sip:bob@127.0.0.1:5080;transport=tcp>\r\n\
             Content-Length: 0\r\n\r\n",
            bridge.port()
        );
        sofia.write_all(request.as_bytes()).expect("write REGISTER");

        // Pump the main loop until the reply arrives, with a hard budget so a
        // regression fails the test instead of hanging the suite.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut inbound = Vec::new();
        let reply = loop {
            assert!(Instant::now() < deadline, "timed out waiting for the reply");
            main.iteration(false);

            let mut chunk = [0u8; 4096];
            match sofia.read(&mut chunk) {
                Ok(0) => panic!("bridge closed the connection"),
                Ok(n) => inbound.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("read from bridge failed: {e}"),
            }
            if let Some(msg) = take_sip_message(&mut inbound) {
                break String::from_utf8(msg).expect("utf-8 reply");
            }
            std::thread::sleep(Duration::from_millis(5));
        };

        // ── What the server saw ───────────────────────────────────────────
        let sent = received.recv_timeout(Duration::from_secs(1)).expect("server got a message");
        assert!(
            sent.contains("Via: SIP/2.0/WS ") && sent.contains(".invalid;branch=z9hG4bKtest"),
            "Via must be rewritten to a WS sent-by: {sent}"
        );
        assert!(
            sent.contains(".invalid;transport=ws>"),
            "Contact must advertise an unroutable WS host: {sent}"
        );
        assert!(!sent.contains("127.0.0.1"), "no loopback address may leak: {sent}");
        assert!(!sent.contains("Route:"), "the bridge consumes its own Route: {sent}");

        // ── What sofia got back ───────────────────────────────────────────
        assert!(
            reply.contains("Via: SIP/2.0/TCP 127.0.0.1:5080;branch=z9hG4bKtest\r\n"),
            "Via must be restored to sofia's own sent-by: {reply}"
        );
        assert!(
            reply.contains("Contact: <sip:bob@pbx.example.com;transport=tcp>\r\n"),
            "the peer's WS contact must be mapped back to TCP: {reply}"
        );
    }
}
