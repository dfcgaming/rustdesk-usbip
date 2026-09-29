// Windows side of the RemoteUsb feature's `usbip`-CLI touches.
//
// Split by direction:
// - USB/IP *client* (vhci attach/detach, `usbip attach`/`detach`/`port`):
//   maps to `usbip-win2` (https://github.com/vadimgrn/usbip-win2), whose CLI
//   keeps the upstream Linux `usbip` syntax (`usbip -t <port> attach -r
//   <host> -b <busid>`) and whose `usbip port` prints the same
//   `Port NN: ... -> usbip://host:port/busid` records the Rust-side parsers
//   already key on.
// - USB/IP *server* (share/unshare, the Linux `usbip bind`/`unbind` calls):
//   maps to `usbipd-win` (https://github.com/dorssel/usbipd-win), which
//   serves the same USB/IP wire protocol on the same TCP 3240 the Rust-side
//   relays dial. Its device state is consumed as `usbipd state`'s JSON, so
//   no free-text format is parsed.
use base::message_proto::UsbDevice;
use hbb_common::log;
use std::{collections::HashSet, process::Command};

const USBIPD_WIN_DIR: &str = "usbipd-win";
const USBIP_CLIENT_DIR: &str = "USBip";

fn find_binaries(dir: &str, name: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    for var in ["ProgramFiles", "ProgramW6432", "ProgramData"] {
        if let Some(base) = std::env::var_os(var) {
            let mut path = std::path::PathBuf::from(base).join(dir).join(name);
            path.set_extension("exe");
            if let Some(s) = path.to_str() {
                candidates.push(s.to_string());
            }
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        let exe = format!("{}.exe", name);
        for dir in std::env::split_paths(&paths) {
            if let Some(p) = dir.join(&exe).to_str().map(str::to_string) {
                candidates.push(p);
            }
        }
    }
    candidates
}

fn find_usbipd() -> Option<String> {
    find_binaries(USBIPD_WIN_DIR, "usbipd")
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
}

fn find_usbip() -> Option<String> {
    find_binaries(USBIP_CLIENT_DIR, "usbip")
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
}

fn is_elevated() -> bool {
    crate::platform::is_elevated(None).unwrap_or(false)
}

fn powershell_path() -> String {
    format!(
        "{root}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
        root = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string())
    )
}

fn quote(token: &str) -> String {
    if token.is_empty() || token.bytes().any(|b| b <= b' ') {
        format!("\"{}\"", token)
    } else {
        token.to_string()
    }
}

/// The Linux callers pass `usbip`-shaped argument lists; on Windows each
/// subcommand maps to a different binary underneath, while the argument
/// tokens themselves stay validated (bus id, port number).
fn map_args(args: &[&str]) -> (Option<String>, Vec<String>) {
    match args {
        ["bind", "-b", id] | ["unbind", "-b", id] => {
            let sub = args[0];
            (find_usbipd(), vec![sub.to_string(), format!("--busid={}", id)])
        }
        ["detach", "-p", port] => (
            find_usbip(),
            vec![
                "detach".to_string(),
                "-p".to_string(),
                (*port).to_string(),
            ],
        ),
        _ => (find_usbip(), args.iter().map(|s| s.to_string()).collect()),
    }
}

fn run_process(exe: &str, args: &[String]) -> Option<std::process::Output> {
    Command::new(exe).args(args).output().ok()
}

/// One UAC prompt for a `cmd.exe` child that runs `cmd` (its own tokens must
/// be validated or already quoted; its stdout/stderr goes through cmd-level
/// `>` redirection, since UAC-launched children cannot inherit our console).
fn run_elevated_wait(cmd: &str) -> bool {
    if cmd.contains('\'') {
        log::error!("usbip: elevated command contains a quote: {:?}", cmd);
        return false;
    }
    match Command::new(powershell_path())
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!(
            "Start-Process -FilePath cmd.exe -ArgumentList '/C','{}' -Verb RunAs -Wait",
            cmd
        ))
        .status()
    {
        Ok(status) => {
            if !status.success() {
                log::error!("usbip: UAC-run cmd.exe failed: {:?}", cmd);
            }
            status.success()
        }
        Err(err) => {
            log::error!("usbip: failed to run powershell: {}", err);
            false
        }
    }
}

/// Blocking; call via `spawn_blocking`. Same contract as
/// `platform::linux::run_usbip_privileged`: `true` when the mapped
/// privileged operation finished successfully. The RustDesk *service*
/// (SYSTEM, the controlled side in practice) runs directly; the desktop
/// client (non-elevated) goes through one UAC prompt instead, and the side
/// effect cannot be verified from inside it -- matching the Linux sudo
/// behavior where the caller relies only on success/failure reporting.
pub fn run_usbip_privileged(args: &[&str]) -> bool {
    let (exe, mapped_args) = map_args(args);
    let Some(exe) = exe else {
        log::error!("usbip: {} tool not found on this machine", args[0]);
        return false;
    };
    let command = format!(
        "\"{}\" {}",
        exe,
        mapped_args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
    );
    if is_elevated() {
        let Some(output) = run_process(&exe, &mapped_args) else {
            log::error!("usbip: failed to spawn {}: {}", exe, command);
            return false;
        };
        if !output.status.success() {
            log::error!(
                "usbip: {} failed: {}{}",
                command,
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        return output.status.success();
    }
    run_elevated_wait(&command)
}

/// Blocking; call via `spawn_blocking`. Same contract as
/// `platform::linux::run_usbip_attach_privileged`: attach through the
/// loopback relay on `listener_port`, returning the combined
/// attach/`usbip port` output (the `Port NN: ... -> usbip://...` record the
/// parse uses). UAC case: the cmd.exe child writes the combined output to a
/// file this process pre-created, so it can read it back regardless of which
/// administrator identity the prompt ends up in.
pub fn run_usbip_attach_privileged(listener_port: u16, bus_id: &str) -> Option<String> {
    let exe = find_usbip()?;
    let port_str = listener_port.to_string();
    if is_elevated() {
        let attach_args = vec![
            "-t".to_string(),
            port_str.clone(),
            "attach".to_string(),
            "-r".to_string(),
            "127.0.0.1".to_string(),
            "-b".to_string(),
            bus_id.to_string(),
        ];
        let attach_out = run_process(&exe, &attach_args)?;
        if !attach_out.status.success() {
            log::error!(
                "usb attach: attach failed: {}{}",
                String::from_utf8_lossy(&attach_out.stdout).trim(),
                String::from_utf8_lossy(&attach_out.stderr).trim()
            );
            return None;
        }
        let mut combined = String::new();
        if let Ok(port_out) = Command::new(&exe).arg("port").output() {
            combined.push_str(&String::from_utf8_lossy(&port_out.stdout));
        }
        combined.push_str(&String::from_utf8_lossy(&attach_out.stdout));
        return Some(combined);
    }
    let out_file =
        std::env::temp_dir().join(format!("rustdesk-usbip-{}-{}.txt", std::process::id(), listener_port));
    if std::fs::write(&out_file, b"").is_err() {
        log::error!("usb attach: failed to create {:?}", out_file);
        return None;
    }
    let out_path = out_file.to_string_lossy().to_string();
    let cmd = format!(
        "\"{}\" -t {} attach -r 127.0.0.1 -b {} > \"{}\" 2>&1 & \"{}\" port >> \"{}\" 2>&1",
        exe, port_str, bus_id, out_path, exe, out_path
    );
    if !run_elevated_wait(&cmd) {
        return None;
    }
    std::fs::read_to_string(&out_file).ok()
}

#[derive(serde::Deserialize)]
struct UsbipdDevice {
    #[serde(rename = "BusId")]
    bus_id: String,
    #[serde(rename = "InstanceId")]
    instance_id: String,
    #[serde(rename = "StubInstanceId")]
    stub_instance_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct UsbipdState {
    #[serde(rename = "Devices", default)]
    devices: Vec<UsbipdDevice>,
}

/// Blocking; call via `spawn_blocking`. Runs the non-privileged `usbipd
/// state` and parses its JSON output; no free-text output format involved.
fn usbipd_state() -> Option<UsbipdState> {
    let exe = find_usbipd()?;
    let output = Command::new(exe).arg("state").output().ok()?;
    match serde_json::from_slice::<UsbipdState>(&output.stdout) {
        Ok(state) => Some(state),
        Err(err) => {
            log::error!("usbip: failed to parse usbipd state output: {}", err);
            None
        }
    }
}

/// Vendor/product ids out of the InstanceId `USB\VID_XXXX&PID_XXXX\...`
/// registry string, keeping the `vendor`/`product` fields' `{:04x}-string`
/// shape the Linux sysfs/`usbip list` producers emit.
fn ids_from_instance_id(instance_id: &str) -> (String, String) {
    let upper = instance_id.to_uppercase();
    let vendor = upper
        .split_once("VID_")
        .map(|(_, r)| r.split('&').next().unwrap_or(""))
        .unwrap_or("");
    let product = upper
        .split_once("PID_")
        .map(|(_, r)| r.split('\\').next().unwrap_or(""))
        .unwrap_or("");
    (vendor.to_string(), product.to_string())
}

pub(crate) fn list_local_devices_impl() -> Vec<UsbDevice> {
    let Some(state) = usbipd_state() else {
        return Vec::new();
    };
    let shared = shared_bus_ids_impl();
    state
        .devices
        .into_iter()
        .map(|d| {
            let (vendor, product) = ids_from_instance_id(&d.instance_id);
            UsbDevice {
                shared: shared.contains(&d.bus_id),
                bus_id: d.bus_id,
                vendor,
                product,
                attached_port: -1,
                ..Default::default()
            }
        })
        .collect()
}

/// Bus ids currently bound by `usbipd bind`: a bound device owns a non-null
/// `StubInstanceId` in `usbipd state` until it is unbound (or attached).
pub(crate) fn shared_bus_ids_impl() -> HashSet<String> {
    let Some(state) = usbipd_state() else {
        return Default::default();
    };
    state
        .devices
        .into_iter()
        .filter(|d| d.stub_instance_id.is_some())
        .map(|d| d.bus_id)
        .collect()
}
