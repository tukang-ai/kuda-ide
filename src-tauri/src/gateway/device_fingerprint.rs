use sha2::{Digest, Sha256};
use std::sync::Mutex;
use sysinfo::{Networks, System};

use crate::error::{AppError, Result};

#[derive(Debug, Clone)]
pub struct HardwareComponents {
    pub cpu_brand: String,
    pub hostname: String,
    pub mac_addresses: Vec<String>,
    pub os_serial: String,
    pub boot_uuid: String,
}

pub struct DeviceFingerprint {
    cached_hash: Mutex<Option<String>>,
}

impl DeviceFingerprint {
    pub fn new() -> Self {
        Self {
            cached_hash: Mutex::new(None),
        }
    }

    pub fn compute_current_hash(&self) -> Result<String> {
        if let Ok(guard) = self.cached_hash.lock() {
            if let Some(ref hash) = *guard {
                return Ok(hash.clone());
            }
        }

        let comps = Self::collect_components()?;
        let mut hasher = Sha256::new();
        hasher.update(comps.cpu_brand.as_bytes());
        hasher.update(comps.hostname.as_bytes());

        for mac in &comps.mac_addresses {
            hasher.update(mac.as_bytes());
        }

        hasher.update(comps.os_serial.as_bytes());
        hasher.update(comps.boot_uuid.as_bytes());

        let hash = format!("{:x}", hasher.finalize());

        if let Ok(mut guard) = self.cached_hash.lock() {
            *guard = Some(hash.clone());
        }

        Ok(hash)
    }

    pub fn verify(&self, expected_hash: &str) -> Result<()> {
        let current = self.compute_current_hash()?;
        if current != expected_hash {
            return Err(AppError::DeviceMismatch);
        }
        Ok(())
    }

    fn collect_components() -> Result<HardwareComponents> {
        let mut sys = System::new_all();
        sys.refresh_all();

        let cpu_brand = sys
            .cpus()
            .first()
            .map(|c| c.brand().to_string())
            .unwrap_or_else(|| "Unknown CPU".to_string());

        let hostname = System::host_name().unwrap_or_else(|| "kuda-host".to_string());

        let networks = Networks::new_with_refreshed_list();
        let mut mac_addresses: Vec<String> = networks
            .iter()
            .map(|(_name, net)| net.mac_address().to_string())
            .filter(|mac| mac != "00:00:00:00:00:00" && !mac.is_empty())
            .collect();
        mac_addresses.sort();

        let os_serial = Self::read_macos_serial().unwrap_or_else(|| "DEFAULT_OS_SERIAL".to_string());
        let boot_uuid = System::kernel_version().unwrap_or_else(|| "DEFAULT_BOOT_UUID".to_string());

        Ok(HardwareComponents {
            cpu_brand,
            hostname,
            mac_addresses,
            os_serial,
            boot_uuid,
        })
    }

    #[cfg(target_os = "macos")]
    fn read_macos_serial() -> Option<String> {
        use std::process::Command;
        let output = Command::new("ioreg")
            .args(["-l", "-d", "1", "-c", "IOPlatformExpertDevice"])
            .output()
            .ok()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.contains("IOPlatformSerialNumber") {
                if let Some(val) = line.split('=').nth(1) {
                    return Some(val.trim().trim_matches('"').to_string());
                }
            }
        }
        None
    }

    #[cfg(not(target_os = "macos"))]
    fn read_macos_serial() -> Option<String> {
        None
    }
}
