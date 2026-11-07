// SPDX-License-Identifier: GPL-3.0-or-later
//! The web phone: answer and make a paired phone's calls from a browser on
//! this PC's network (`https://<this PC>:53330`), with the call's sound in
//! the browser while the phone is plugged into this PC ([`crate::call_audio`]).
//!
//! Security model:
//! - Off by default; on, it listens on every network of this PC but serves
//!   only LAN (and tailnet) addresses (`is_lan_ip`).
//! - HTTPS with a self-signed certificate made here (browsers only give a
//!   page the microphone over HTTPS); a browser asks once to trust it.
//! - Everything but the page itself needs the 256-bit access key the PC shows
//!   in its link (`#k=…`), compared in constant time. The key and certificate
//!   live in `<data_dir>/web-phone.json`; turning the web phone off deletes
//!   it, so a new link (and key) comes with the next start.

use std::{
    collections::HashMap,
    fs,
    net::{IpAddr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use data_encoding::{BASE64, HEXLOWER};
use nectarlink_core::{CallCommand, DeviceId, LinkState};
use ring::{
    rand::{SecureRandom, SystemRandom},
    signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
    task::JoinHandle,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};

use crate::{call_audio, calls, core_host, settings::Settings};

/// The default port (next to the local automation API's 53320).
pub const DEFAULT_PORT: u16 = 53330;
const STATE_FILE: &str = "web-phone.json";
const PAGE: &str = include_str!("../web/phone.html");
/// The longest request head a browser sends here.
const MAX_HEAD: usize = 16 * 1024;
/// The longest WebSocket message taken (a second of sound is 32 KB).
const MAX_FRAME: usize = 64 * 1024;

#[derive(Serialize, Deserialize, Clone)]
struct StateFile {
    key: String,
    cert_der_hex: String,
    key_pkcs8_hex: String,
    /// The addresses the certificate names.
    ips: Vec<IpAddr>,
}

struct Running {
    port: u16,
    task: JoinHandle<()>,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

fn running<T>(f: impl FnOnce(&mut Option<Running>) -> T) -> T {
    f(&mut RUNNING.lock().unwrap_or_else(|e| e.into_inner()))
}

fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join(STATE_FILE)
}

/// The port the settings ask for.
pub fn configured_port() -> u16 {
    core_host::try_data_dir()
        .map(Settings::load)
        .map(|s| s.web_phone_port)
        .filter(|p| *p != 0)
        .unwrap_or(DEFAULT_PORT)
}

/// The link to open on another device, with the access key; "" while off.
pub fn url() -> String {
    let Some(data_dir) = core_host::try_data_dir() else { return String::new() };
    let settings = Settings::load(data_dir);
    if !settings.web_phone_enabled {
        return String::new();
    }
    let Some(state) = load_state(data_dir) else { return String::new() };
    let host = lan_ip().map(|ip| ip.to_string()).unwrap_or_else(|| "localhost".into());
    let port = if settings.web_phone_port == 0 { DEFAULT_PORT } else { settings.web_phone_port };
    // In the query, not a #fragment: QR scanners and the browser's
    // certificate warning drop fragments. The page takes it out of the
    // address bar once it has it.
    format!("https://{host}:{port}/?k={}", state.key)
}

/// Starts or stops the web phone as `desktop-settings.json` says.
pub fn sync_from_settings() {
    let data_dir = core_host::host().data_dir.clone();
    apply(&data_dir, &Settings::load(&data_dir));
}

/// Starts the web phone on startup when it's on.
pub fn init(settings: &Settings) {
    apply(&core_host::host().data_dir, settings);
}

/// Stops the web phone.
pub fn stop() {
    if let Some(old) = running(Option::take) {
        old.task.abort();
    }
}

fn apply(data_dir: &Path, settings: &Settings) {
    let port = if settings.web_phone_port == 0 { DEFAULT_PORT } else { settings.web_phone_port };
    if !settings.web_phone_enabled {
        stop();
        let _ = fs::remove_file(state_path(data_dir));
        return;
    }
    let state = match ensure_state(data_dir) {
        Ok(state) => state,
        Err(e) => {
            tracing::warn!(error = %e, "can't set up the web phone");
            return;
        }
    };
    if running(|r| r.as_ref().is_some_and(|r| r.port == port && !r.task.is_finished())) {
        return;
    }
    let tls = match server_config(&state) {
        Ok(tls) => Arc::new(tls),
        Err(e) => {
            tracing::warn!(error = %e, "web phone certificate unusable");
            return;
        }
    };
    stop();
    let key: Arc<str> = state.key.into();
    let task = core_host::host().runtime.spawn(async move {
        let listener = match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(port, error = %e, "can't start the web phone");
                return;
            }
        };
        tracing::info!(port, "web phone listening");
        serve(listener, TlsAcceptor::from(tls), key).await;
    });
    running(|r| *r = Some(Running { port, task }));
}

// ---- Key and certificate ----

fn load_state(data_dir: &Path) -> Option<StateFile> {
    let text = fs::read_to_string(state_path(data_dir)).ok()?;
    let state: StateFile = serde_json::from_str(&text).ok()?;
    (state.key.len() == 64 && server_config(&state).is_ok()).then_some(state)
}

/// The stored key and certificate, made anew when missing or when this PC's
/// address is no longer one the certificate names.
fn ensure_state(data_dir: &Path) -> std::io::Result<StateFile> {
    let ips = cert_ips();
    let old = load_state(data_dir);
    if let Some(old) = &old
        && ips.iter().all(|ip| old.ips.contains(ip))
    {
        return Ok(old.clone());
    }
    let rng = SystemRandom::new();
    let key = match old {
        Some(old) => old.key,
        None => {
            let mut bytes = [0u8; 32];
            rng.fill(&mut bytes).map_err(|_| std::io::Error::other("no randomness"))?;
            HEXLOWER.encode(&bytes)
        }
    };
    let (cert, pkcs8) = self_signed(&ips).map_err(std::io::Error::other)?;
    let state = StateFile { key, cert_der_hex: HEXLOWER.encode(&cert), key_pkcs8_hex: HEXLOWER.encode(&pkcs8), ips };
    fs::create_dir_all(data_dir)?;
    let path = state_path(data_dir);
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&state).map_err(std::io::Error::other)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, &path)?;
    Ok(state)
}

fn cert_ips() -> Vec<IpAddr> {
    let mut ips = vec![IpAddr::from([127, 0, 0, 1])];
    ips.extend(lan_ip());
    ips
}

/// This PC's address on the network its default route goes through (no
/// packet is sent: connecting a UDP socket only picks the route).
fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect(("192.0.2.1", 9)).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

fn server_config(state: &StateFile) -> Result<ServerConfig, String> {
    let cert = HEXLOWER.decode(state.cert_der_hex.as_bytes()).map_err(|e| e.to_string())?;
    let key = HEXLOWER.decode(state.key_pkcs8_hex.as_bytes()).map_err(|e| e.to_string())?;
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        )
        .map_err(|e| e.to_string())
}

/// A self-signed ECDSA P-256 certificate for `localhost` and `ips`:
/// (certificate DER, PKCS#8 key).
fn self_signed(ips: &[IpAddr]) -> Result<(Vec<u8>, Vec<u8>), String> {
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng)
        .map_err(|_| "can't make a key")?
        .as_ref()
        .to_vec();
    let pair =
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &pkcs8, &rng).map_err(|_| "can't read the key")?;
    let mut serial = [0u8; 16];
    rng.fill(&mut serial).map_err(|_| "no randomness")?;
    serial[0] = (serial[0] & 0x7F) | 0x01;

    let sig_alg = der_seq(&[der_tlv(0x06, &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02])]);
    let cn = der_seq(&[der_tlv(0x06, &[0x55, 0x04, 0x03]), der_tlv(0x0C, b"NectarLink web phone")]);
    let name = der_seq(&[der_tlv(0x31, &cn)]);
    let validity = der_seq(&[der_tlv(0x17, b"250101000000Z"), der_tlv(0x17, b"350101000000Z")]);
    let spki = der_seq(&[
        der_seq(&[
            der_tlv(0x06, &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01]),
            der_tlv(0x06, &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07]),
        ]),
        der_bit_string(pair.public_key().as_ref()),
    ]);
    // subjectAltName (2.5.29.17): dNSName [2] and iPAddress [7].
    let mut names = vec![der_tlv(0x82, b"localhost")];
    for ip in ips {
        match ip {
            IpAddr::V4(v4) => names.push(der_tlv(0x87, &v4.octets())),
            IpAddr::V6(v6) => names.push(der_tlv(0x87, &v6.octets())),
        }
    }
    let san = der_seq(&[der_tlv(0x06, &[0x55, 0x1D, 0x11]), der_tlv(0x04, &der_seq(&names))]);
    let extensions = der_tlv(0xA3, &der_seq(&[san]));
    let version = der_tlv(0xA0, &der_tlv(0x02, &[0x02]));
    let tbs = der_seq(&[
        version,
        der_tlv(0x02, &serial),
        sig_alg.clone(),
        name.clone(),
        validity,
        name,
        spki,
        extensions,
    ]);
    let signature = pair.sign(&rng, &tbs).map_err(|_| "can't sign")?;
    Ok((der_seq(&[tbs, sig_alg, der_bit_string(signature.as_ref())]), pkcs8))
}

fn der_tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let len = body.len();
    let mut out = Vec::with_capacity(4 + len);
    out.push(tag);
    if len < 128 {
        out.push(len as u8);
    } else if len <= 0xFF {
        out.extend([0x81, len as u8]);
    } else {
        out.extend([0x82, (len >> 8) as u8, len as u8]);
    }
    out.extend_from_slice(body);
    out
}

fn der_seq(items: &[Vec<u8>]) -> Vec<u8> {
    der_tlv(0x30, &items.concat())
}

fn der_bit_string(bytes: &[u8]) -> Vec<u8> {
    let mut body = vec![0u8];
    body.extend_from_slice(bytes);
    der_tlv(0x03, &body)
}

// ---- HTTP ----

async fn serve(listener: TcpListener, tls: TlsAcceptor, key: Arc<str>) {
    loop {
        let Ok((tcp, peer)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        };
        if !nectarlink_core::is_lan_ip(peer.ip()) {
            continue;
        }
        let (tls, key) = (tls.clone(), key.clone());
        tokio::spawn(async move {
            let _ = tcp.set_nodelay(true);
            let Ok(Ok(stream)) = tokio::time::timeout(Duration::from_secs(10), tls.accept(tcp)).await else {
                return;
            };
            if let Err(e) = connection(stream, peer, &key).await {
                tracing::debug!(%peer, error = %e, "web phone connection ended");
            }
        });
    }
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
}

/// Serves requests on one connection until it closes or becomes a WebSocket.
async fn connection<S>(mut stream: S, peer: SocketAddr, key: &str) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut buf = Vec::new();
    loop {
        let Some(request) = read_request(&mut stream, &mut buf).await? else { return Ok(()) };
        if request.path != "/api/state" {
            tracing::info!(%peer, method = %request.method, path = %request.path, "web phone request");
        }
        let authorized = request.query.get("k").is_some_and(|k| crate::local_api::constant_time_eq(k, key));
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/") | ("GET", "/index.html") => {
                // The page itself needs no key; its link carries one in `?k=`.
                respond(&mut stream, 200, "text/html; charset=utf-8", PAGE.as_bytes()).await?;
            }
            (_, "/api/state" | "/api/action" | "/api/dial" | "/api/audio") if !authorized => {
                tokio::time::sleep(Duration::from_millis(300)).await;
                respond_json(&mut stream, 401, &json!({ "error": "wrong key" })).await?;
            }
            ("GET", "/api/state") => respond_json(&mut stream, 200, &state_json()).await?,
            ("POST", "/api/action") => {
                let action = request.query.get("action").map(String::as_str).unwrap_or("");
                let (status, body) = action_json(action, request.query.get("digit").map(String::as_str)).await;
                respond_json(&mut stream, status, &body).await?;
            }
            ("POST", "/api/dial") => {
                let number = request.query.get("number").map(String::as_str).unwrap_or("");
                let (status, body) = dial_json(number).await;
                respond_json(&mut stream, status, &body).await?;
            }
            ("GET", "/api/audio") => {
                let Some(ws_key) = request.headers.get("sec-websocket-key").cloned() else {
                    return respond_json(&mut stream, 400, &json!({ "error": "not a WebSocket" })).await;
                };
                let Some((device, _, _)) = calls::active_or_ringing(phone()).filter(|(_, _, ringing)| !ringing)
                else {
                    return respond_json(&mut stream, 409, &json!({ "error": "no call in progress" })).await;
                };
                let line = match call_audio::open(device).await {
                    Ok(line) => line,
                    Err(e) => return respond_json(&mut stream, 409, &json!({ "error": e })).await,
                };
                let accept = websocket_accept(&ws_key);
                let head = format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                );
                stream.write_all(head.as_bytes()).await?;
                tracing::info!(%peer, "web phone audio connected");
                return audio(stream, line).await;
            }
            _ => respond_json(&mut stream, 404, &json!({ "error": "not found" })).await?,
        }
    }
}

/// Reads one request head (and skips its body); `None` when the connection closed.
async fn read_request<S: AsyncRead + Unpin>(stream: &mut S, buf: &mut Vec<u8>) -> std::io::Result<Option<Request>> {
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            return Err(std::io::Error::other("request too large"));
        }
        let mut chunk = [0u8; 4096];
        let n = match tokio::time::timeout(Duration::from_secs(60), stream.read(&mut chunk)).await {
            Ok(n) => n?,
            Err(_) => return Ok(None),
        };
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    buf.drain(..end);
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_owned();
    let target = first.next().unwrap_or("/");
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let body_len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if body_len > MAX_HEAD {
        return Err(std::io::Error::other("body too large"));
    }
    while buf.len() < body_len {
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    buf.drain(..body_len);
    let (path, query) = split_target(target);
    Ok(Some(Request { method, path, query, headers }))
}

fn split_target(target: &str) -> (String, HashMap<String, String>) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect();
    (path.to_owned(), query)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => match bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                Some(b) => {
                    out.push(b);
                    i += 2;
                }
                None => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn respond<S: AsyncWrite + Unpin>(stream: &mut S, status: u16, kind: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'self'; script-src 'self' 'unsafe-inline' blob:; style-src 'unsafe-inline'; \
         connect-src 'self' wss:; img-src 'self' data:; frame-ancestors 'none'\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

async fn respond_json<S: AsyncWrite + Unpin>(stream: &mut S, status: u16, body: &Value) -> std::io::Result<()> {
    respond(stream, status, "application/json", body.to_string().as_bytes()).await
}

// ---- The phone and its calls ----

/// The phone the web phone works with: one plugged into this PC, else the
/// first one online.
fn phone() -> Option<DeviceId> {
    let devices = core_host::host().hub.read(|s| {
        s.devices.iter().map(|d| (d.id, matches!(d.link, LinkState::Online { .. }))).collect::<Vec<_>>()
    });
    devices
        .iter()
        .find(|(id, _)| call_audio::available(*id))
        .or_else(|| devices.iter().find(|(_, online)| *online))
        .map(|(id, _)| *id)
}

fn state_json() -> Value {
    let device = phone();
    let phone = device.map(|id| {
        let (name, online) = core_host::host().hub.read(|s| {
            let d = s.devices.iter().find(|d| d.id == id);
            (
                d.map(|d| d.info.name.clone()).unwrap_or_default(),
                d.is_some_and(|d| matches!(d.link, LinkState::Online { .. })),
            )
        });
        json!({
            "name": name,
            "online": online,
            "usb": call_audio::available(id),
            "can_control": calls::can_control(id),
            "can_dial": calls::can_dial(id),
        })
    });
    let call = calls::active_or_ringing(device).map(|(_, call, ringing)| {
        let mut value = serde_json::to_value(&call).unwrap_or(Value::Null);
        if let Some(map) = value.as_object_mut() {
            map.remove("photo");
            map.insert("ringing".into(), Value::Bool(ringing));
        }
        value
    });
    json!({ "phone": phone, "call": call })
}

async fn action_json(action: &str, digit: Option<&str>) -> (u16, Value) {
    let Some(command) = CallCommand::parse(action, digit) else {
        return (400, json!({ "error": "unknown action" }));
    };
    let Some((device, call, _)) = calls::active_or_ringing(phone()) else {
        return (409, json!({ "error": "no call" }));
    };
    let Some(node) = core_host::node() else { return (409, json!({ "error": "not running" })) };
    match node.call_command(device, call.id, command).await {
        Ok(()) => (200, json!({ "ok": true })),
        Err(e) => (409, json!({ "error": e.to_string() })),
    }
}

async fn dial_json(number: &str) -> (u16, Value) {
    let number = number.trim();
    let valid = !number.is_empty()
        && number.len() <= 32
        && number.chars().all(|c| c.is_ascii_digit() || matches!(c, '+' | '*' | '#'));
    if !valid {
        return (400, json!({ "error": "not a phone number" }));
    }
    let Some(device) = phone() else { return (409, json!({ "error": "no phone" })) };
    let Some(node) = core_host::node() else { return (409, json!({ "error": "not running" })) };
    match node.call_dial(device, number.to_owned(), None).await {
        Ok(()) => (200, json!({ "ok": true })),
        Err(e) => (409, json!({ "error": e.to_string() })),
    }
}

// ---- The call's sound over a WebSocket ----

fn websocket_accept(key: &str) -> String {
    let digest = ring::digest::digest(
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
    );
    BASE64.encode(digest.as_ref())
}

/// Binary messages both ways: 16 kHz mono 16-bit little-endian PCM.
async fn audio<S>(stream: S, line: Arc<call_audio::Line>) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (out, mut outgoing) = mpsc::channel::<(u8, Vec<u8>)>(64);
    let send_task = tokio::spawn(async move {
        while let Some((opcode, payload)) = outgoing.recv().await {
            if writer.write_all(&ws_frame(opcode, &payload)).await.is_err() {
                break;
            }
            if opcode == 0x8 {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });
    let mut heard = line.listen();
    let to_browser = out.clone();
    let listen_task = tokio::spawn(async move {
        loop {
            match heard.recv().await {
                Ok(pcm) => {
                    if to_browser.send((0x2, pcm.to_vec())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    let _ = to_browser.send((0x8, 1000u16.to_be_bytes().to_vec())).await;
                    break;
                }
            }
        }
    });
    let mut from_browser = 0u64;
    let result = async {
        loop {
            let (opcode, payload) = read_ws_frame(&mut reader).await?;
            match opcode {
                0x2 => {
                    from_browser += payload.len() as u64;
                    line.say(payload);
                }
                0x8 => {
                    let _ = out.send((0x8, Vec::new())).await;
                    return Ok(());
                }
                0x9 => {
                    let _ = out.send((0xA, payload)).await;
                }
                _ => {}
            }
        }
    }
    .await;
    listen_task.abort();
    drop(out);
    let _ = send_task.await;
    tracing::info!(from_browser, "web phone audio closed");
    result
}

fn ws_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x80 | opcode);
    match payload.len() {
        n if n < 126 => frame.push(n as u8),
        n if n <= 0xFFFF => {
            frame.push(126);
            frame.extend((n as u16).to_be_bytes());
        }
        n => {
            frame.push(127);
            frame.extend((n as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(payload);
    frame
}

async fn read_ws_frame<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    reader.read_exact(&mut head).await?;
    let opcode = head[0] & 0x0F;
    let masked = head[1] & 0x80 != 0;
    let len = match head[1] & 0x7F {
        126 => {
            let mut b = [0u8; 2];
            reader.read_exact(&mut b).await?;
            u16::from_be_bytes(b) as usize
        }
        127 => {
            let mut b = [0u8; 8];
            reader.read_exact(&mut b).await?;
            usize::try_from(u64::from_be_bytes(b)).unwrap_or(usize::MAX)
        }
        n => n as usize,
    };
    if !masked || len > MAX_FRAME {
        return Err(std::io::Error::other("bad WebSocket frame"));
    }
    let mut mask = [0u8; 4];
    reader.read_exact(&mut mask).await?;
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= mask[i % 4];
    }
    Ok((opcode, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_accept_matches_rfc_6455() {
        assert_eq!(websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn targets_and_escapes() {
        let (path, query) = split_target("/api/dial?k=ab&number=%2B84%20123&x");
        assert_eq!(path, "/api/dial");
        assert_eq!(query["number"], "+84 123");
        assert_eq!(query["k"], "ab");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
    }

    #[test]
    fn the_certificate_builds_a_tls_config() {
        let (cert, key) = self_signed(&[IpAddr::from([192, 168, 1, 5])]).unwrap();
        let state = StateFile {
            key: "0".repeat(64),
            cert_der_hex: HEXLOWER.encode(&cert),
            key_pkcs8_hex: HEXLOWER.encode(&key),
            ips: vec![],
        };
        assert!(server_config(&state).is_ok());
    }

    #[test]
    fn frames_round_trip() {
        let frame = ws_frame(0x2, &[1, 2, 3]);
        assert_eq!(frame, vec![0x82, 3, 1, 2, 3]);
        assert_eq!(ws_frame(0x2, &vec![0; 300])[1], 126);
    }
}
