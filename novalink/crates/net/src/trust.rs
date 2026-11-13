//! Persistent list of paired devices (JSON, atomic 0600 saves).
use anyhow::{Context, Result};
use novalink_proto::DeviceId;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

const MAX_ADDRS: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedDevice {
    pub id: DeviceId,
    pub name: String,
    /// Most recent first.
    pub last_addrs: Vec<SocketAddr>,
}

pub struct TrustStore {
    path: PathBuf,
    devices: Vec<PairedDevice>,
}

impl TrustStore {
    /// Open the store; a missing file is an empty store.
    pub fn open(path: &Path) -> Result<Self> {
        let devices = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        Ok(Self {
            path: path.to_path_buf(),
            devices,
        })
    }

    pub fn is_trusted(&self, id: &DeviceId) -> bool {
        self.devices.iter().any(|d| d.id == *id)
    }

    /// Add or replace a device (same id).
    pub fn add(&mut self, device: PairedDevice) -> Result<()> {
        match self.devices.iter_mut().find(|d| d.id == device.id) {
            Some(existing) => *existing = device,
            None => self.devices.push(device),
        }
        self.save()
    }

    pub fn remove(&mut self, id: &DeviceId) -> Result<()> {
        self.devices.retain(|d| d.id != *id);
        self.save()
    }

    /// Record a working address: most recent first, deduplicated, at most 4.
    /// Unknown ids are ignored.
    pub fn note_addr(&mut self, id: &DeviceId, addr: SocketAddr) -> Result<()> {
        let Some(d) = self.devices.iter_mut().find(|d| d.id == *id) else {
            return Ok(());
        };
        d.last_addrs.retain(|a| *a != addr);
        d.last_addrs.insert(0, addr);
        d.last_addrs.truncate(MAX_ADDRS);
        self.save()
    }

    pub fn list(&self) -> Vec<PairedDevice> {
        self.devices.clone()
    }

    pub fn find_by_name(&self, name: &str) -> Option<PairedDevice> {
        self.devices.iter().find(|d| d.name == name).cloned()
    }

    fn save(&self) -> Result<()> {
        let dir = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        let json = serde_json::to_vec_pretty(&self.devices)?;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let tmp = self
            .path
            .with_extension(format!("tmp{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let result = (|| -> Result<()> {
            let mut f = opts.open(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.with_context(|| format!("save {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use novalink_proto::DeviceId;
    #[test]
    fn trust_store_roundtrip_and_perms() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let p = dir.path().join("paired.json");
        let mut t = TrustStore::open(&p)?;
        let id = DeviceId([9; 32]);
        t.add(PairedDevice {
            id,
            name: "pc".into(),
            last_addrs: vec![],
        })?;
        for port in 1..=6 {
            t.note_addr(&id, format!("192.168.1.5:{port}").parse()?)?;
        }
        let t2 = TrustStore::open(&p)?;
        assert!(t2.is_trusted(&id));
        assert_eq!(t2.list()[0].last_addrs.len(), 4);
        assert_eq!(t2.find_by_name("pc").map(|d| d.id), Some(id));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p)?.permissions().mode() & 0o777, 0o600);
        }
        Ok(())
    }
    #[test]
    fn note_addr_dedupes_keeps_recent_first() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut t = TrustStore::open(&dir.path().join("p.json"))?;
        let id = DeviceId([1; 32]);
        t.add(PairedDevice {
            id,
            name: "a".into(),
            last_addrs: vec![],
        })?;
        for port in [1, 2, 1] {
            t.note_addr(&id, format!("10.0.0.1:{port}").parse()?)?;
        }
        let addrs = &t.list()[0].last_addrs;
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0].port(), 1);
        t.remove(&id)?;
        assert!(!t.is_trusted(&id));
        Ok(())
    }
}
