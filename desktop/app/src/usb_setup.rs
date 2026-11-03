// SPDX-License-Identifier: GPL-3.0-or-later
//! Plugging a phone in: what it still needs, in plain words, and the button
//! that does it from here.
//!
//! A worker looks every two seconds at adb (starting its server, or fetching
//! Google's platform-tools, pinned, when there is none), lists USB phones
//! including the ones that haven't allowed this PC yet, and checks whether
//! NectarLink is installed on them. The page offers installing the APK this
//! PC carries and opening the app. Once the app answers over USB, the core's
//! own USB tunnel, pairing and Elevated over USB take over, and the phone
//! drops off this list.

use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use cxx_qt::CxxQtThread;
use cxx_qt_lib::QString;
use nectarlink_adb::{AdbDevice, DEFAULT_ADB_ADDR};
use nectarlink_core::{ConnectionPath, LinkState};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::bridge::app::qobject::AppController;

const PACKAGE: &str = "app.nectarlink";
const MAIN_ACTIVITY: &str = "app.nectarlink/app.nectarlink.android.ui.MainActivity";
const POLL: Duration = Duration::from_secs(2);
/// How long a phone's "is NectarLink installed" answer is trusted.
const APP_CHECK_TTL: Duration = Duration::from_secs(10);
/// How long a phone with NectarLink installed may stay unconnected over USB
/// before the app is opened on it from here, and how often that's retried.
const AUTO_OPEN_AFTER: Duration = Duration::from_secs(6);
const AUTO_OPEN_EVERY: Duration = Duration::from_secs(60);

/// Google's platform-tools r37.0.1; SHA-1 checked against Google's
/// repository2-3.xml, SHA-256 pinned here.
#[cfg(target_os = "macos")]
const PLATFORM_TOOLS: (&str, &str) = (
    "https://dl.google.com/android/repository/platform-tools_r37.0.1-darwin.zip",
    "ee39ad5967e95c2a07f04dbcbde96b1a0c916ba376096db5d2f498b7727a5d1d",
);
#[cfg(windows)]
const PLATFORM_TOOLS: (&str, &str) = (
    "https://dl.google.com/android/repository/platform-tools_r37.0.1-win.zip",
    "45f4d63113e895ebde0c90f194099a4676b6ac653bd28d54314a9e022bbc1a99",
);
#[cfg(not(any(target_os = "macos", windows)))]
const PLATFORM_TOOLS: (&str, &str) = ("", "");

/// Appium's UiAutomator2 server, which Claude's mobile-lite MCP reads the
/// screen with; pinned like mobile-mcp's scripts/setup.sh.
const UIA2_PACKAGE: &str = "io.appium.uiautomator2.server";
const UIA2_URL: &str = "https://github.com/appium/appium-uiautomator2-server/releases/download/v10.6.6";
const UIA2_APKS: [(&str, &str); 2] = [
    ("appium-uiautomator2-server-v10.6.6.apk", "8ff760a2a86b487f53090fbdcd5b0360e67d02bb811887d527a9557b0d59c80d"),
    (
        "appium-uiautomator2-server-debug-androidTest.apk",
        "e6f729287d72351388fd66597c35b69988b9091a7011954bf92a55d416cb1e1c",
    ),
];
/// How long the "is mobile-lite registered" answer is trusted.
const CLAUDE_CHECK_TTL: Duration = Duration::from_secs(60);

#[derive(Serialize, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
struct View {
    /// "ok", "missing" (no adb anywhere), "downloading", "failed".
    adb: &'static str,
    adb_error: String,
    /// Can this PC fetch platform-tools by itself.
    can_download_adb: bool,
    /// Whether this PC carries a NectarLink APK to install.
    has_apk: bool,
    /// A paired phone is connected over USB right now.
    usb_connected: bool,
    /// Claude Code is installed here, and mobile-lite is registered with it.
    claude_installed: bool,
    claude_registered: bool,
    phones: Vec<Phone>,
}

#[derive(Serialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Phone {
    serial: String,
    model: String,
    /// adb's state: "device", "unauthorized", "offline", "no permissions"...
    state: String,
    /// "missing", "installed", "unknown".
    app: &'static str,
    /// Whether the UiAutomator2 server Claude reads the screen with is on it.
    claude_ready: bool,
    busy: bool,
    error: String,
}

#[derive(Default)]
struct Shared {
    downloading: bool,
    adb_error: String,
    busy: HashMap<String, bool>,
    errors: HashMap<String, String>,
    apps: HashMap<String, (bool, Instant)>,
    /// When each phone was first seen waiting for its app, and last opened.
    waiting_since: HashMap<String, Instant>,
    uia2: HashMap<String, (bool, Instant)>,
    claude: Option<(bool, bool, Instant)>,
    auto_opened: HashMap<String, Instant>,
    last: Option<View>,
}

fn shared() -> std::sync::MutexGuard<'static, Shared> {
    static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
    SHARED.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("usb setup runtime")
    })
}

/// Starts the worker that keeps `usbSetup` up to date.
pub fn start(qt: CxxQtThread<AppController>) {
    let _ = std::thread::Builder::new().name("usb-setup".into()).spawn(move || loop {
        let view = std::panic::catch_unwind(gather).unwrap_or_default();
        let changed = {
            let mut s = shared();
            let changed = s.last.as_ref() != Some(&view);
            s.last = Some(view.clone());
            changed
        };
        if changed {
            let json = serde_json::to_string(&view).unwrap_or_else(|_| "{}".into());
            if qt.queue(move |object| object.set_usb_setup(QString::from(&json))).is_err() {
                return;
            }
        }
        std::thread::sleep(POLL);
    });
}

fn gather() -> View {
    let adb = ensure_adb_server();
    let (downloading, adb_error) = {
        let s = shared();
        (s.downloading, s.adb_error.clone())
    };
    let mut view = View {
        adb: if downloading {
            "downloading"
        } else if adb {
            "ok"
        } else if adb_error.is_empty() {
            "missing"
        } else {
            "failed"
        },
        adb_error,
        can_download_adb: !PLATFORM_TOOLS.0.is_empty(),
        has_apk: apk_path().is_some(),
        usb_connected: usb_connected(),
        claude_installed: false,
        claude_registered: false,
        phones: Vec::new(),
    };
    (view.claude_installed, view.claude_registered) = claude_status();
    if !adb {
        return view;
    }
    let devices = runtime()
        .block_on(async { tokio::time::timeout(Duration::from_secs(5), nectarlink_adb::list_devices(DEFAULT_ADB_ADDR)).await })
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let plugged: Vec<String> = devices.iter().map(|d| d.serial.clone()).collect();
    {
        let mut s = shared();
        s.waiting_since.retain(|serial, _| plugged.contains(serial));
        s.auto_opened.retain(|serial, _| plugged.contains(serial));
    }
    for device in devices.into_iter().filter(is_usb_phone) {
        let app = if device.is_online() { app_installed(&device.serial) } else { "unknown" };
        if app == "installed" && !view.usb_connected {
            auto_open(&device.serial);
        } else {
            shared().waiting_since.remove(&device.serial);
        }
        let claude_ready = device.is_online() && uia2_installed(&device.serial);
        let s = shared();
        view.phones.push(Phone {
            model: device.model.clone().unwrap_or_default().replace('_', " "),
            state: device.state.clone(),
            app,
            claude_ready,
            busy: s.busy.get(&device.serial).copied().unwrap_or(false),
            error: s.errors.get(&device.serial).cloned().unwrap_or_default(),
            serial: device.serial,
        });
    }
    view
}

/// Opens NectarLink on a phone that has it but hasn't connected over USB for a
/// few seconds and isn't running it (just updated, restarted, or stopped):
/// plugging in is enough.
fn auto_open(serial: &str) {
    let due = {
        let mut s = shared();
        let since = *s.waiting_since.entry(serial.to_owned()).or_insert_with(Instant::now);
        let recent = s.auto_opened.get(serial).is_some_and(|at| at.elapsed() < AUTO_OPEN_EVERY);
        let busy = s.busy.get(serial).copied().unwrap_or(false);
        let due = since.elapsed() >= AUTO_OPEN_AFTER && !recent && !busy;
        if due {
            s.auto_opened.insert(serial.to_owned(), Instant::now());
        }
        due
    };
    // Only when it isn't running: a running app may be pairing (a new PC has
    // nothing to connect to yet), and bringing it forward would end that.
    if due && !app_running(serial) {
        tracing::info!(serial, "opening NectarLink on the plugged-in phone");
        act(serial.to_owned(), "open".to_owned());
    }
}

fn app_running(serial: &str) -> bool {
    shell(serial, &format!("pidof {PACKAGE}")).is_ok_and(|out| !out.trim().is_empty())
}

/// Phones on a cable; adb also lists wireless-debugging and emulator serials.
fn is_usb_phone(device: &AdbDevice) -> bool {
    !device.serial.starts_with("emulator-") && !device.serial.contains(':') && !device.serial.contains("._adb-tls")
}

fn usb_connected() -> bool {
    crate::core_host::try_data_dir().is_some()
        && crate::core_host::host().hub.read(|s| {
            s.devices.iter().any(|d| matches!(d.link, LinkState::Online { path: ConnectionPath::Usb, .. }))
        })
}

fn app_installed(serial: &str) -> &'static str {
    if let Some((installed, at)) = shared().apps.get(serial).copied()
        && at.elapsed() < APP_CHECK_TTL
    {
        return if installed { "installed" } else { "missing" };
    }
    match shell(serial, &format!("pm path {PACKAGE}")) {
        Ok(out) => {
            let installed = out.contains("package:");
            shared().apps.insert(serial.to_owned(), (installed, Instant::now()));
            if installed { "installed" } else { "missing" }
        }
        Err(_) => "unknown",
    }
}

/// Runs a short command on the phone and returns what it printed.
fn shell(serial: &str, command: &str) -> Result<String, String> {
    runtime().block_on(async {
        let run = async {
            let mut stream = nectarlink_adb::shell(DEFAULT_ADB_ADDR, serial, command).await.map_err(|e| e.to_string())?;
            let mut out = Vec::new();
            stream.read_to_end(&mut out).await.map_err(|e| e.to_string())?;
            Ok(String::from_utf8_lossy(&out).into_owned())
        };
        tokio::time::timeout(Duration::from_secs(15), run).await.map_err(|_| "timed out".to_owned())?
    })
}

/// A button on the USB card: "download-adb", "install", "open", "reconnect".
pub fn act(serial: String, action: String) {
    if action != "download-adb" && !nectarlink_adb::is_serial_allowed(&serial) {
        return;
    }
    {
        let mut s = shared();
        if action == "download-adb" {
            if s.downloading {
                return;
            }
            s.downloading = true;
            s.adb_error.clear();
        } else {
            if s.busy.get(&serial).copied().unwrap_or(false) {
                return;
            }
            s.busy.insert(serial.clone(), true);
            s.errors.remove(&serial);
        }
    }
    let _ = std::thread::Builder::new().name("usb-setup-action".into()).spawn(move || {
        let result = match action.as_str() {
            "download-adb" => download_adb(),
            "install" => install_apk(&serial).and_then(|()| open_app(&serial)),
            "open" => open_app(&serial),
            "reconnect" => reconnect(),
            "claude" => set_up_claude(&serial),
            _ => Ok(()),
        };
        let mut s = shared();
        if action == "download-adb" {
            s.downloading = false;
            if let Err(e) = result {
                tracing::warn!(error = e, "couldn't set up adb");
                s.adb_error = e;
            }
        } else {
            s.busy.remove(&serial);
            s.apps.remove(&serial);
            s.uia2.remove(&serial);
            s.claude = None;
            if let Err(e) = result {
                tracing::warn!(serial, action, error = e, "usb setup action failed");
                s.errors.insert(serial, e);
            }
        }
    });
}

fn open_app(serial: &str) -> Result<(), String> {
    let out = shell(serial, &format!("am start -n {MAIN_ACTIVITY}"))?;
    if out.contains("Error") { Err(out.trim().to_owned()) } else { Ok(()) }
}

fn reconnect() -> Result<(), String> {
    runtime().block_on(async {
        let mut stream = tokio::net::TcpStream::connect(DEFAULT_ADB_ADDR).await.map_err(|e| e.to_string())?;
        nectarlink_adb::send_command(&mut stream, "host:reconnect-offline").await.map_err(|e| e.to_string())
    })
}

/// Installs the NectarLink APK this PC carries.
fn install_apk(serial: &str) -> Result<(), String> {
    let path = apk_path().ok_or("This PC has no NectarLink app for phones to install.")?;
    install_file(serial, &path, "")
}

/// Streams an APK to the phone's package manager (`adb install`, streamed);
/// `flags` go before `-S` (`-t` for a test APK).
fn install_file(serial: &str, path: &Path, flags: &str) -> Result<(), String> {
    let apk = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let out = runtime().block_on(async {
        let run = async {
            let mut stream =
                tokio::net::TcpStream::connect(DEFAULT_ADB_ADDR).await.map_err(|e| e.to_string())?;
            nectarlink_adb::send_command(&mut stream, &format!("host:transport:{serial}"))
                .await
                .map_err(|e| e.to_string())?;
            nectarlink_adb::send_command(&mut stream, &format!("exec:cmd package install -r {flags} -S {}", apk.len()))
                .await
                .map_err(|e| e.to_string())?;
            stream.write_all(&apk).await.map_err(|e| e.to_string())?;
            stream.flush().await.map_err(|e| e.to_string())?;
            let mut out = Vec::new();
            stream.read_to_end(&mut out).await.map_err(|e| e.to_string())?;
            Ok::<_, String>(String::from_utf8_lossy(&out).into_owned())
        };
        tokio::time::timeout(Duration::from_secs(180), run).await.map_err(|_| "timed out".to_owned())?
    })?;
    if out.contains("Success") {
        tracing::info!(serial, apk = %path.display(), "installed over USB");
        Ok(())
    } else {
        Err(out.trim().to_owned())
    }
}

/// The phone app this PC carries: next to the program (Windows, Linux), in the
/// bundle's Resources (macOS), or `NECTARLINK_PHONE_APK`.
fn apk_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NECTARLINK_PHONE_APK").map(PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    [exe_dir.join("nectarlink.apk"), exe_dir.join("../Resources/nectarlink.apk")].into_iter().find(|p| p.is_file())
}

// ---- Claude -----------------------------------------------------------------------------

fn uia2_installed(serial: &str) -> bool {
    if let Some((installed, at)) = shared().uia2.get(serial).copied()
        && at.elapsed() < APP_CHECK_TTL
    {
        return installed;
    }
    let installed = shell(serial, &format!("pm list packages {UIA2_PACKAGE}")).is_ok_and(|out| {
        let has = |pkg: String| out.lines().any(|l| l.trim() == pkg);
        has(format!("package:{UIA2_PACKAGE}")) && has(format!("package:{UIA2_PACKAGE}.test"))
    });
    shared().uia2.insert(serial.to_owned(), (installed, Instant::now()));
    installed
}

/// (Claude Code found, mobile-lite registered with it), checked once a minute.
fn claude_status() -> (bool, bool) {
    if let Some((installed, registered, at)) = shared().claude
        && at.elapsed() < CLAUDE_CHECK_TTL
    {
        return (installed, registered);
    }
    let installed = find_tool("claude").is_some();
    let registered = installed && mobile_lite_registered();
    shared().claude = Some((installed, registered, Instant::now()));
    (installed, registered)
}

/// Installs UiAutomator2 on the phone and registers mobile-lite with Claude
/// Code, so Claude can see and use this phone (mobile-mcp's setup, from here).
fn set_up_claude(serial: &str) -> Result<(), String> {
    let dir = tools_dir().ok_or("NectarLink isn't ready yet")?.join("uiautomator2");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for (name, sha256) in UIA2_APKS {
        let file = dir.join(name);
        if !file.is_file() || sha256_of(&file)? != sha256 {
            fetch(&format!("{UIA2_URL}/{name}"), &file)?;
            let got = sha256_of(&file)?;
            if got != sha256 {
                let _ = std::fs::remove_file(&file);
                return Err(format!("{name} didn't match the expected checksum ({got})"));
            }
        }
        install_file(serial, &file, "-t")?;
    }
    register_mobile_lite()
}

/// Whether Claude Code's user config has mobile-lite (`claude mcp get` would
/// start the server to check its health, too much for a poll).
fn mobile_lite_registered() -> bool {
    let Some(config) = dirs::home_dir().map(|h| h.join(".claude.json")) else { return false };
    std::fs::read_to_string(config)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .is_some_and(|json| json.get("mcpServers").and_then(|m| m.get("mobile-lite")).is_some())
}

fn register_mobile_lite() -> Result<(), String> {
    let claude = find_tool("claude").ok_or("Install Claude Code first, then try again.")?;
    if mobile_lite_registered() {
        return Ok(());
    }
    let server = mobile_mcp_dir()
        .map(|d| d.join("lib").join("index.js"))
        .filter(|p| p.is_file())
        .ok_or("mobile-mcp isn't built here: set MOBILE_MCP_DIR to its folder (with lib/index.js).")?;
    let node = find_tool("node").ok_or("Install Node.js first, then try again.")?;
    let status = hidden(Command::new(&claude).args(["mcp", "add", "-s", "user", "mobile-lite", "--"]).arg(node).arg(server))
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err(format!("claude mcp add failed ({status})")) }
}

/// mobile-mcp's checkout: `MOBILE_MCP_DIR`, or where it lives on this Mac.
fn mobile_mcp_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("MOBILE_MCP_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = dirs::home_dir()?;
    [home.join("Documents/research-repo/mobile-dev-tools/mobile-mcp"), home.join("mobile-mcp")]
        .into_iter()
        .find(|d| d.join("lib").join("index.js").is_file())
}

/// A program on the PATH or where installers put it (a GUI app on macOS
/// doesn't get the shell's PATH).
fn find_tool(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".claude/local"));
        dirs.push(home.join("AppData/Roaming/npm"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{name}.exe"), format!("{name}.cmd")]
    } else {
        vec![name.to_owned()]
    };
    dirs.into_iter().flat_map(|d| names.iter().map(move |n| d.join(n))).find(|p| p.is_file())
}

// ---- adb --------------------------------------------------------------------------------

/// Whether an adb server answers, starting one from the first adb found.
fn ensure_adb_server() -> bool {
    if std::net::TcpStream::connect_timeout(&DEFAULT_ADB_ADDR, Duration::from_millis(300)).is_ok() {
        return true;
    }
    let Some(adb) = find_adb() else { return false };
    // ADB_MDNS=0: no mDNS discovery (wireless debugging, which this doesn't
    // use), so adb listens on loopback only and Windows Firewall doesn't ask.
    let started = hidden(Command::new(&adb).arg("start-server").env("ADB_MDNS", "0"))
        .status()
        .is_ok_and(|s| s.success());
    if !started {
        tracing::warn!(adb = %adb.display(), "adb start-server failed");
    }
    started
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_owned() }
}

/// adb from the platform-tools this app fetched, next to the program, on the
/// PATH, or in an Android SDK.
fn find_adb() -> Option<PathBuf> {
    let adb = exe("adb");
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = tools_dir() {
        dirs.push(dir.join("platform-tools"));
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        dirs.push(dir.join("platform-tools"));
        dirs.push(dir);
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(sdk) = std::env::var_os(var) {
            dirs.push(PathBuf::from(sdk).join("platform-tools"));
        }
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join("Library/Android/sdk/platform-tools"));
        dirs.push(home.join("Android/Sdk/platform-tools"));
        // A GUI app on macOS doesn't get the shell's PATH.
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
    }
    if let Some(local) = dirs::data_local_dir() {
        dirs.push(local.join("Android/Sdk/platform-tools"));
    }
    dirs.into_iter().map(|d| d.join(&adb)).find(|p| p.is_file())
}

/// Where fetched platform-tools live.
fn tools_dir() -> Option<PathBuf> {
    crate::core_host::try_data_dir().map(|d| d.join("tools"))
}

fn download_adb() -> Result<(), String> {
    let (url, sha256) = PLATFORM_TOOLS;
    if url.is_empty() {
        return Err("Install adb with your package manager (android-tools).".into());
    }
    let dir = tools_dir().ok_or("NectarLink isn't ready yet")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let zip = dir.join("platform-tools.zip");
    let _ = std::fs::remove_file(&zip);
    fetch(url, &zip)?;
    let got = sha256_of(&zip)?;
    if got != sha256 {
        let _ = std::fs::remove_file(&zip);
        return Err(format!("platform-tools didn't match the expected checksum ({got})"));
    }
    let _ = std::fs::remove_dir_all(dir.join("platform-tools"));
    unzip(&zip, &dir)?;
    let _ = std::fs::remove_file(&zip);
    if ensure_adb_server() { Ok(()) } else { Err("adb didn't start".into()) }
}

#[cfg(any(windows, target_os = "macos"))]
fn fetch(url: &str, file: &Path) -> Result<(), String> {
    crate::win::http::download(url, file, 64 * 1024 * 1024, |_, _| {})
}

#[cfg(not(any(windows, target_os = "macos")))]
fn fetch(url: &str, file: &Path) -> Result<(), String> {
    let status = Command::new("curl")
        .args(["-fsSL", "--proto", "=https", "-o"])
        .arg(file)
        .arg(url)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err(format!("download failed ({status})")) }
}

fn unzip(zip: &Path, into: &Path) -> Result<(), String> {
    // Both ship with the OS: bsdtar on Windows 10+, ditto on macOS.
    let mut command = if cfg!(target_os = "macos") {
        let mut c = Command::new("/usr/bin/ditto");
        c.arg("-x").arg("-k").arg(zip).arg(into);
        c
    } else {
        let mut c = Command::new("tar");
        c.arg("-xf").arg(zip).arg("-C").arg(into);
        c
    };
    let status = hidden(&mut command).status().map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err(format!("couldn't unpack platform-tools ({status})")) }
}

fn sha256_of(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// No console window flashing up for adb on Windows.
fn hidden(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
}
