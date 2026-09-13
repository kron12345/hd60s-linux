//! How the service runs and whether the card may be opened: systemd,
//! our own child process, the autostart choices, the udev access check,
//! and the program's own little configuration file.

use std::path::PathBuf;

use crate::{UDEV_RULE, UDEV_RULE_PATH};

// ------------------------------------------------------------ device access

/// Whether a card is on the bus and whether this user may open it.
pub(crate) enum Access {
    NoCard,
    Ok,
    Denied(String),
}

pub(crate) fn device_access() -> Access {
    let Ok(entries) = std::fs::read_dir("/sys/bus/usb/devices") else {
        return Access::NoCard;
    };
    let read = |p: PathBuf| {
        std::fs::read_to_string(p)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if read(path.join("idVendor")) != "0fd9" {
            continue;
        }
        if !["004f", "005e", "0074", "0076"].contains(&read(path.join("idProduct")).as_str()) {
            continue;
        }
        let (bus, dev) = (read(path.join("busnum")), read(path.join("devnum")));
        let (Ok(bus), Ok(dev)) = (bus.parse::<u32>(), dev.parse::<u32>()) else {
            continue;
        };
        let node = format!("/dev/bus/usb/{bus:03}/{dev:03}");
        let c = std::ffi::CString::new(node.clone()).unwrap();
        // SAFETY: access() only inspects permissions of the given path.
        let ok = unsafe { libc::access(c.as_ptr(), libc::R_OK | libc::W_OK) } == 0;
        return if ok { Access::Ok } else { Access::Denied(node) };
    }
    Access::NoCard
}

pub(crate) fn udev_rule_installed() -> bool {
    [
        "/etc/udev/rules.d/70-hd60s-linux.rules",
        "/usr/lib/udev/rules.d/70-hd60s-linux.rules",
    ]
    .iter()
    .any(|p| std::path::Path::new(p).exists())
}

/// Writes the embedded rule through polkit (which asks for an
/// administrator's password) and reloads udev. Without administrator
/// rights the rule is saved in the user's config directory with the
/// command an administrator needs.
pub(crate) fn install_udev_rule() -> Result<String, String> {
    let script = format!(
        "printf '%s' \"$1\" > {UDEV_RULE_PATH} && udevadm control --reload-rules && udevadm trigger --subsystem-match=usb"
    );
    let status = std::process::Command::new("pkexec")
        .args(["sh", "-c", &script, "sh", UDEV_RULE])
        .status();
    if matches!(&status, Ok(s) if s.success()) {
        return Ok(format!(
            "access rule installed at {UDEV_RULE_PATH}; unplug and plug the card once"
        ));
    }
    let copy = config_dir().join("hd60s-linux/70-hd60s-linux.rules");
    if let Some(dir) = copy.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&copy, UDEV_RULE);
    Err(format!(
        "not installed ({}). The rule is saved at {}; an administrator installs it with: sudo install -m644 {} {UDEV_RULE_PATH} && sudo udevadm control --reload-rules",
        match status {
            Ok(_) => "cancelled or no administrator rights".to_string(),
            Err(e) => format!("pkexec: {e}"),
        },
        copy.display(),
        copy.display()
    ))
}

// ------------------------------------------------------------- service run

/// `systemctl --user` for the service.
pub(crate) fn systemctl(args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .arg("hd60s-serve.service")
        .output()
        .map_err(|e| format!("systemctl: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() || args[0].starts_with("is-") {
        Ok(text)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// The service run by this program when systemd is not running it.
pub struct OwnService(std::process::Child);

impl Drop for OwnService {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(crate) fn start_own_service() -> Result<OwnService, String> {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new("hd60s-linux");
    command
        .args(["serve", "--tray", "off"])
        .stdin(std::process::Stdio::null());
    // SAFETY: prctl only marks the child to receive SIGTERM when this
    // process dies, however it dies — the service must never outlive the
    // program that started it.
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    command
        .spawn()
        .map(OwnService)
        .map_err(|e| format!("starting hd60s-linux serve: {e}"))
}

// --------------------------------------------------------------- settings

pub(crate) fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `key=value` lines; only `close_to_tray` so far.
pub(crate) fn read_config() -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(config_dir().join("hd60s-linux/control.conf"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

pub(crate) fn write_config(key: &str, value: &str) {
    let mut config = read_config();
    config.insert(key.to_string(), value.to_string());
    let path = config_dir().join("hd60s-linux/control.conf");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = config
        .iter()
        .map(|(k, v)| format!("{k}={v}\n"))
        .collect::<String>();
    let _ = std::fs::write(path, text);
}

pub(crate) fn autostart_file() -> PathBuf {
    config_dir().join("autostart/hd60s-control.desktop")
}

/// 0 none, 1 with the desktop (XDG autostart), 2 systemd user service.
pub(crate) fn autostart_mode() -> usize {
    if autostart_file().exists() {
        1
    } else if systemctl(&["is-enabled"]).as_deref() == Ok("enabled") {
        2
    } else {
        0
    }
}

pub(crate) fn set_autostart(mode: usize) -> Result<String, String> {
    let file = autostart_file();
    match mode {
        1 => {
            let _ = systemctl(&["disable", "--now"]);
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            std::fs::write(
                &file,
                "[Desktop Entry]\nType=Application\nName=HD60 S Control\nExec=hd60s-control --tray\nIcon=hd60s-control\nX-GNOME-Autostart-enabled=true\n",
            )
            .map_err(|e| e.to_string())?;
            Ok("starts with the desktop, minimised to the tray (sway: add `exec hd60s-control --tray` to your config)".into())
        }
        2 => {
            let _ = std::fs::remove_file(&file);
            systemctl(&["unmask"])?;
            systemctl(&["enable", "--now"])?;
            Ok("systemd user service enabled: runs at login and with the card, even without this program".into())
        }
        _ => {
            let _ = std::fs::remove_file(&file);
            let _ = systemctl(&["disable", "--now"]);
            Ok("no autostart: the service runs while this program is open".into())
        }
    }
}
