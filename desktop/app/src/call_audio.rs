// SPDX-License-Identifier: GPL-3.0-or-later
//! The sound of a phone call on this PC, for the web phone ([`crate::web_phone`]).
//!
//! While a phone plugged into this PC is on a call, the PC starts the app's
//! `CallAudio` helper on it over the cable (as the shell user, the way
//! scrcpy starts its server): the helper writes what the other party says
//! to its stdout and plays what it reads from stdin into the call, both as
//! 16 kHz mono 16-bit little-endian PCM. One helper serves the call; every
//! listener hears it and the one talker's sound goes into it.

use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use nectarlink_core::DeviceId;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{broadcast, mpsc},
};

use crate::core_host;

/// The app on the phone (the helper's classes come from its APK).
const PACKAGE: &str = "app.nectarlink";
const HELPER_CLASS: &str = "app.nectarlink.android.elevated.CallAudio";
/// Where the helper says why it ended, for `adb shell cat`.
const HELPER_LOG: &str = "/data/local/tmp/nl-call-audio.log";
/// The helper's sound format.
pub const RATE: u32 = 16_000;
/// 20 ms of sound, in bytes.
const CHUNK: usize = (RATE as usize / 50) * 2;

/// A call's sound while its helper runs.
pub struct Line {
    pub device: DeviceId,
    downlink: broadcast::Sender<Arc<Vec<u8>>>,
    uplink: mpsc::Sender<Vec<u8>>,
    /// Bytes of the PC's sound taken, and dropped because the phone fell behind.
    taken: AtomicU64,
    dropped: AtomicU64,
}

impl Line {
    /// What the other party says, 20 ms at a time; closed when the call ends.
    pub fn listen(&self) -> broadcast::Receiver<Arc<Vec<u8>>> {
        self.downlink.subscribe()
    }

    /// Plays `pcm` into the call (dropped when the phone falls behind).
    pub fn say(&self, pcm: Vec<u8>) {
        let len = pcm.len() as u64;
        if self.uplink.try_send(pcm).is_ok() {
            self.taken.fetch_add(len, Ordering::Relaxed);
        } else {
            self.dropped.fetch_add(len, Ordering::Relaxed);
        }
    }
}

static LINE: Mutex<Option<Arc<Line>>> = Mutex::new(None);

fn current() -> Option<Arc<Line>> {
    LINE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Whether `device` is plugged into this PC, so its calls' sound can come here.
pub fn available(device: DeviceId) -> bool {
    core_host::node().is_some_and(|n| n.usb_serial(&device).is_some())
}

/// The sound of `device`'s call in progress, starting its helper if needed.
pub async fn open(device: DeviceId) -> Result<Arc<Line>, String> {
    if let Some(line) = current().filter(|l| l.device == device && !l.uplink.is_closed()) {
        return Ok(line);
    }
    let node = core_host::node().ok_or("NectarLink isn't running")?;
    let (serial, adb) = node.usb_serial(&device).ok_or("Plug the phone into this computer with its USB cable")?;
    let apk = apk_path(adb, &serial).await?;
    // Its stderr says why it ended (and how much of the PC's sound it played).
    let command = format!("CLASSPATH={apk} app_process / {HELPER_CLASS} 2>{HELPER_LOG}");
    let stream = nectarlink_adb::shell(adb, &serial, &command).await.map_err(|e| e.to_string())?;
    let (mut from_phone, mut to_phone) = stream.into_split();
    let (downlink, _) = broadcast::channel(50);
    let (uplink, mut said) = mpsc::channel::<Vec<u8>>(25);
    let line = Arc::new(Line {
        device,
        downlink: downlink.clone(),
        uplink,
        taken: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
    });
    *LINE.lock().unwrap_or_else(|e| e.into_inner()) = Some(line.clone());
    tracing::info!(peer = %device.short(), "call audio started");

    // With `<data_dir>/call-audio-debug` present (and only then), both
    // directions of this call go to `<data_dir>/logs/call-{downlink,uplink}.raw`.
    let dump = |name: &str| {
        let dir = core_host::try_data_dir()?;
        dir.join("call-audio-debug").exists().then(|| std::fs::File::create(dir.join("logs").join(name)).ok())?
    };
    let (mut dump_down, mut dump_up) = (dump("call-downlink.raw"), dump("call-uplink.raw"));
    let ours = Arc::downgrade(&line);
    let written = Arc::new(AtomicU64::new(0));
    let sent = written.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match from_phone.read_exact(&mut buf).await {
                Ok(_) => {
                    if let Some(f) = dump_down.as_mut() {
                        let _ = std::io::Write::write_all(f, &buf);
                    }
                    let _ = downlink.send(Arc::new(buf.clone()));
                }
                Err(_) => break,
            }
        }
        let (taken, dropped) = ours.upgrade().map_or((0, 0), |l| {
            (l.taken.load(Ordering::Relaxed), l.dropped.load(Ordering::Relaxed))
        });
        tracing::info!(peer = %device.short(), taken, dropped, written = written.load(Ordering::Relaxed), "call audio ended");
        let mut slot = LINE.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|l| ours.upgrade().is_some_and(|o| Arc::ptr_eq(l, &o))) {
            *slot = None;
        }
    });
    tokio::spawn(async move {
        // How evenly the PC's sound arrives: long gaps empty the phone's buffer.
        let (mut last, mut longest, mut gaps) = (None::<std::time::Instant>, 0u128, 0u32);
        while let Some(pcm) = said.recv().await {
            let now = std::time::Instant::now();
            if let Some(last) = last {
                let gap = now.duration_since(last).as_millis();
                longest = longest.max(gap);
                gaps += u32::from(gap > 100);
            }
            last = Some(now);
            if let Some(f) = dump_up.as_mut() {
                let _ = std::io::Write::write_all(f, &pcm);
            }
            if let Err(e) = to_phone.write_all(&pcm).await {
                tracing::info!(error = %e, "call audio: the phone stopped taking sound");
                break;
            }
            sent.fetch_add(pcm.len() as u64, Ordering::Relaxed);
        }
        tracing::info!(longest_gap_ms = longest as u64, gaps_over_100ms = gaps, "call audio: PC sound pacing");
    });
    Ok(line)
}

/// Where the app's APK is installed, checked to be a plain path.
async fn apk_path(adb: SocketAddr, serial: &str) -> Result<String, String> {
    let mut stream =
        nectarlink_adb::shell(adb, serial, &format!("pm path {PACKAGE}")).await.map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(10), stream.read_to_end(&mut out))
        .await
        .map_err(|_| "the phone didn't answer".to_owned())?
        .map_err(|e| e.to_string())?;
    parse_apk_path(&String::from_utf8_lossy(&out)).ok_or_else(|| "NectarLink isn't installed on the phone".to_owned())
}

fn parse_apk_path(out: &str) -> Option<String> {
    let path = out.lines().find_map(|l| l.trim().strip_prefix("package:"))?.trim();
    let plain = path.starts_with('/')
        && path.ends_with(".apk")
        && path.bytes().all(|b| b.is_ascii_alphanumeric() || b"/_.=~+-".contains(&b));
    plain.then(|| path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apk_paths() {
        assert_eq!(
            parse_apk_path("package:/data/app/~~tukj==/app.nectarlink-VAuz==/base.apk\r\n").as_deref(),
            Some("/data/app/~~tukj==/app.nectarlink-VAuz==/base.apk")
        );
        assert_eq!(parse_apk_path("package:/data/app/x;reboot/base.apk"), None);
        assert_eq!(parse_apk_path(""), None);
    }
}
