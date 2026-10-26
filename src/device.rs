//! Identifies optical drives through sysfs, so a drive can be found again after the kernel
//! renumbers its /dev/srN node (e.g. when USB devices reconnect), without asking MakeMKV.

use anyhow::{Context, Result, bail};
use std::path::Path;
use tokio::process::Command;

pub const SYSFS_ROOT: &str = "/sys";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    // SCSI INQUIRY data, the same model and firmware MakeMKV shows in its drive name.
    pub vendor: String,
    pub model: String,
    pub rev: String,
    // VPD page 0x80. Often not read by the kernel for USB drives.
    pub unit_serial: Option<String>,
    // USB bridge, if the drive is attached over USB.
    pub usb_id: Option<String>,
    pub usb_serial: Option<String>,
    // Host controller plus port chain, without bus numbers, which change if the controller
    // is re-probed.
    pub usb_port: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DeviceLookupError {
    NotFound,
    Ambiguous(Vec<String>),
}

impl std::fmt::Display for DeviceLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceLookupError::NotFound => write!(f, "no matching device found"),
            DeviceLookupError::Ambiguous(paths) => {
                write!(f, "several devices match: {}", paths.join(", "))
            }
        }
    }
}

impl std::error::Error for DeviceLookupError {}

impl std::fmt::Display for DeviceIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} {}", self.vendor, self.model, self.rev)?;
        if let Some(serial) = &self.unit_serial {
            write!(f, " serial={}", serial)?;
        }
        if let Some(id) = &self.usb_id {
            write!(f, " usb={}", id)?;
        }
        if let Some(serial) = &self.usb_serial {
            write!(f, " usb_serial={}", serial)?;
        }
        if let Some(port) = &self.usb_port {
            write!(f, " port={}", port)?;
        }
        Ok(())
    }
}

fn read_attr(dir: &Path, name: &str) -> Option<String> {
    let raw = std::fs::read(dir.join(name)).ok()?;
    let value: String = String::from_utf8_lossy(&raw)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Reads the identity of a block device such as "/dev/sr0".
pub fn identify(sysfs: &Path, device_path: &str) -> Result<DeviceIdentity> {
    let name = Path::new(device_path)
        .file_name()
        .with_context(|| format!("Invalid device path {}", device_path))?;
    let scsi_dir = std::fs::canonicalize(sysfs.join("class/block").join(name).join("device"))
        .with_context(|| format!("No sysfs entry for {}", device_path))?;

    let mut identity = DeviceIdentity {
        vendor: read_attr(&scsi_dir, "vendor").unwrap_or_default(),
        model: read_attr(&scsi_dir, "model").unwrap_or_default(),
        rev: read_attr(&scsi_dir, "rev").unwrap_or_default(),
        // The page starts with a 4-byte header.
        unit_serial: std::fs::read(scsi_dir.join("vpd_pg80"))
            .ok()
            .and_then(|raw| {
                let serial = String::from_utf8_lossy(raw.get(4..)?).trim().to_string();
                (!serial.is_empty()).then_some(serial)
            }),
        usb_id: None,
        usb_serial: None,
        usb_port: None,
    };

    let usb_dir = scsi_dir
        .ancestors()
        .find(|dir| dir.join("idVendor").exists() && dir.join("devpath").exists());
    if let Some(usb_dir) = usb_dir {
        identity.usb_id = Some(format!(
            "{}:{}",
            read_attr(usb_dir, "idVendor").unwrap_or_default(),
            read_attr(usb_dir, "idProduct").unwrap_or_default()
        ));
        identity.usb_serial = read_attr(usb_dir, "serial");
        // Root hubs are named usbN; their parent is the host controller.
        let controller = usb_dir
            .ancestors()
            .find(|dir| {
                dir.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("usb"))
            })
            .and_then(|root_hub| root_hub.parent());
        if let (Some(controller), Some(devpath)) = (controller, read_attr(usb_dir, "devpath")) {
            let controller = controller.strip_prefix(sysfs).unwrap_or(controller);
            identity.usb_port = Some(format!("{}:{}", controller.display(), devpath));
        }
    }

    Ok(identity)
}

/// Lists the optical drives the kernel currently has, with their identities.
pub fn list_optical_drives(sysfs: &Path) -> Result<Vec<(String, DeviceIdentity)>> {
    let mut drives = Vec::new();
    for entry in std::fs::read_dir(sysfs.join("class/block"))? {
        let name = entry?.file_name().to_string_lossy().to_string();
        if !name.starts_with("sr") {
            continue;
        }
        let device_path = format!("/dev/{}", name);
        if let Ok(identity) = identify(sysfs, &device_path) {
            drives.push((device_path, identity));
        }
    }
    drives.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(drives)
}

/// Picks the one current device that is the drive recorded as `identity`. Serial numbers
/// decide when present; the USB port only breaks ties between otherwise identical drives.
pub fn find_device(
    current: &[(String, DeviceIdentity)],
    identity: &DeviceIdentity,
) -> Result<String, DeviceLookupError> {
    let same_drive = |c: &DeviceIdentity| {
        c.vendor == identity.vendor
            && c.model == identity.model
            && c.rev == identity.rev
            && c.usb_id == identity.usb_id
            && c.unit_serial == identity.unit_serial
            && c.usb_serial == identity.usb_serial
    };
    let mut matches: Vec<&(String, DeviceIdentity)> =
        current.iter().filter(|(_, c)| same_drive(c)).collect();
    if matches.len() > 1 && identity.usb_port.is_some() {
        matches.retain(|(_, c)| c.usb_port == identity.usb_port);
    }
    match matches.as_slice() {
        [(path, _)] => Ok(path.clone()),
        [] => Err(DeviceLookupError::NotFound),
        _ => Err(DeviceLookupError::Ambiguous(
            matches.iter().map(|(path, _)| path.clone()).collect(),
        )),
    }
}

/// Reads the drive at `device_path` and checks that its identity picks out only that device,
/// so it can be used to find the drive again later.
pub fn identify_unique(sysfs: &Path, device_path: &str) -> Result<DeviceIdentity> {
    let identity = identify(sysfs, device_path)?;
    let found = find_device(&list_optical_drives(sysfs)?, &identity)?;
    if found != device_path {
        bail!("{} identifies as {}", device_path, found);
    }
    Ok(identity)
}

/// Current device path of the drive recorded as `identity`.
pub fn current_device_path(sysfs: &Path, identity: &DeviceIdentity) -> Result<String> {
    Ok(find_device(&list_optical_drives(sysfs)?, identity)?)
}

pub async fn eject(device_path: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("diskutil");
        command.arg("eject");
        command
    };
    #[cfg(not(target_os = "macos"))]
    let mut command = Command::new("eject");

    let status = command
        .arg(device_path)
        .kill_on_drop(true)
        .status()
        .await
        .with_context(|| format!("Failed to run eject for {}", device_path))?;
    if !status.success() {
        bail!("Eject of {} failed with {}", device_path, status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    struct FakeSysfs {
        root: PathBuf,
    }

    impl FakeSysfs {
        fn new(test: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "auto-makemkv-sysfs-{}-{}",
                std::process::id(),
                test
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("class/block")).unwrap();
            Self { root }
        }

        /// Adds /dev/{sr} as a USB drive at `usb_dev` (e.g. "usb2/2-1/2-1.3") on controller
        /// 0000:00:14.0.
        fn add_usb_drive(
            &self,
            sr: &str,
            usb_dev: &str,
            model: &str,
            usb_serial: Option<&str>,
            scsi_host: u32,
        ) {
            let usb_dir = self
                .root
                .join("devices/pci0000:00/0000:00:14.0")
                .join(usb_dev);
            std::fs::create_dir_all(&usb_dir).unwrap();
            let devpath = usb_dev
                .rsplit('/')
                .next()
                .unwrap()
                .split_once('-')
                .unwrap()
                .1;
            std::fs::write(usb_dir.join("devpath"), format!("{}\n", devpath)).unwrap();
            std::fs::write(usb_dir.join("idVendor"), "152d\n").unwrap();
            std::fs::write(usb_dir.join("idProduct"), "0578\n").unwrap();
            if let Some(serial) = usb_serial {
                std::fs::write(usb_dir.join("serial"), format!("{}\n", serial)).unwrap();
            }
            let usb_name = usb_dev.rsplit('/').next().unwrap();
            let scsi_dir = usb_dir
                .join(format!("{}:1.0", usb_name))
                .join(format!("host{0}/target{0}:0:0/{0}:0:0:0", scsi_host));
            std::fs::create_dir_all(scsi_dir.join("block").join(sr)).unwrap();
            std::fs::write(scsi_dir.join("vendor"), "PIONEER \n").unwrap();
            std::fs::write(scsi_dir.join("model"), format!("{:<16}\n", model)).unwrap();
            std::fs::write(scsi_dir.join("rev"), "1.04\n").unwrap();
            symlink(&scsi_dir, scsi_dir.join("block").join(sr).join("device")).unwrap();
            symlink(
                scsi_dir.join("block").join(sr),
                self.root.join("class/block").join(sr),
            )
            .unwrap();
        }
    }

    impl Drop for FakeSysfs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn reads_usb_drive_identity() {
        let sysfs = FakeSysfs::new("identity");
        sysfs.add_usb_drive("sr0", "usb2/2-1/2-1.3", "BD-RW  BDR-XD08U", Some("AB12"), 6);
        let identity = identify(&sysfs.root, "/dev/sr0").unwrap();
        assert_eq!(identity.vendor, "PIONEER");
        assert_eq!(identity.model, "BD-RW  BDR-XD08U");
        assert_eq!(identity.rev, "1.04");
        assert_eq!(identity.usb_id.as_deref(), Some("152d:0578"));
        assert_eq!(identity.usb_serial.as_deref(), Some("AB12"));
        assert_eq!(
            identity.usb_port.as_deref(),
            Some("devices/pci0000:00/0000:00:14.0:1.3")
        );
    }

    #[test]
    fn finds_drives_by_serial_after_renumbering() {
        let before = FakeSysfs::new("serial-before");
        before.add_usb_drive("sr0", "usb2/2-1/2-1.1", "BD-RE BP60NB10", Some("S-LG"), 6);
        before.add_usb_drive("sr1", "usb2/2-1/2-1.2", "BD-RW  BDR-XD08U", Some("S-P0"), 7);
        let lg = identify_unique(&before.root, "/dev/sr0").unwrap();
        let pioneer = identify_unique(&before.root, "/dev/sr1").unwrap();

        // Controller re-probed with a new bus number, nodes renumbered, and the Pioneer moved
        // to another port.
        let after = FakeSysfs::new("serial-after");
        after.add_usb_drive("sr0", "usb4/4-1/4-1.4", "BD-RW  BDR-XD08U", Some("S-P0"), 8);
        after.add_usb_drive("sr1", "usb4/4-1/4-1.1", "BD-RE BP60NB10", Some("S-LG"), 9);
        assert_eq!(current_device_path(&after.root, &lg).unwrap(), "/dev/sr1");
        assert_eq!(
            current_device_path(&after.root, &pioneer).unwrap(),
            "/dev/sr0"
        );
    }

    #[test]
    fn identical_drives_are_told_apart_by_port() {
        // Same model, and bridges without serial numbers.
        let before = FakeSysfs::new("port-before");
        before.add_usb_drive("sr0", "usb2/2-1/2-1.1", "BD-RW  BDR-XD08U", None, 6);
        before.add_usb_drive("sr1", "usb2/2-1/2-1.2", "BD-RW  BDR-XD08U", None, 7);
        let first = identify_unique(&before.root, "/dev/sr0").unwrap();
        let second = identify_unique(&before.root, "/dev/sr1").unwrap();

        let after = FakeSysfs::new("port-after");
        after.add_usb_drive("sr0", "usb3/3-1/3-1.2", "BD-RW  BDR-XD08U", None, 8);
        after.add_usb_drive("sr2", "usb3/3-1/3-1.1", "BD-RW  BDR-XD08U", None, 9);
        assert_eq!(
            current_device_path(&after.root, &first).unwrap(),
            "/dev/sr2"
        );
        assert_eq!(
            current_device_path(&after.root, &second).unwrap(),
            "/dev/sr0"
        );
    }

    #[test]
    fn refuses_drives_it_cannot_find() {
        let before = FakeSysfs::new("refuse-before");
        before.add_usb_drive("sr0", "usb2/2-1/2-1.1", "BD-RW  BDR-XD08U", None, 6);
        let identity = identify_unique(&before.root, "/dev/sr0").unwrap();

        let gone = FakeSysfs::new("refuse-gone");
        gone.add_usb_drive("sr0", "usb2/2-1/2-1.1", "BD-RE BP60NB10", None, 6);
        assert!(current_device_path(&gone.root, &identity).is_err());

        // Two identical drives without serials, neither on the recorded port.
        let moved = FakeSysfs::new("refuse-moved");
        moved.add_usb_drive("sr0", "usb2/2-1/2-1.5", "BD-RW  BDR-XD08U", None, 6);
        moved.add_usb_drive("sr1", "usb2/2-1/2-1.6", "BD-RW  BDR-XD08U", None, 7);
        let current = list_optical_drives(&moved.root).unwrap();
        assert_eq!(
            find_device(&current, &identity),
            Err(DeviceLookupError::NotFound)
        );

        // Without a recorded port there is nothing to tell them apart.
        let no_port = DeviceIdentity {
            usb_port: None,
            ..identity
        };
        assert_eq!(
            find_device(&current, &no_port),
            Err(DeviceLookupError::Ambiguous(vec![
                "/dev/sr0".to_string(),
                "/dev/sr1".to_string()
            ]))
        );
    }
}
