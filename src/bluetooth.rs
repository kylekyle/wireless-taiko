use std::process::Command;
use anyhow::{Context, Result};
use bluer::{Address, Adapter, Session};
use bluer::rfcomm::{Profile, ProfileHandle};

const SDP_UUID: &str     = "00001000-0000-1000-8000-00805f9b34fb";
const GAMEPAD_CLASS: &str = "0x002508";

pub struct BluetoothAdapter {
    pub adapter:     Adapter,
    _session:        Session,
    _profile_handle: ProfileHandle,
}

impl BluetoothAdapter {
    pub async fn new() -> Result<Self> {
        let session = Session::new().await?;
        let adapter = session.default_adapter().await?;

        remove_known_switches(&adapter).await?;
        prepare_sdp()?;

        adapter.set_powered(true).await?;
        adapter.set_pairable(true).await?;
        adapter.set_pairable_timeout(0).await?;
        adapter.set_discoverable_timeout(180).await?;
        adapter.set_alias("Pro Controller".to_string()).await?;

        let profile = Profile {
            uuid: SDP_UUID.parse().context("invalid SDP UUID")?,
            service_record: Some(include_str!("../sdp_record.xml").to_string()),
            require_authentication: Some(false),
            require_authorization: Some(false),
            auto_connect: Some(true),
            ..Default::default()
        };
        let _profile_handle = session.register_profile(profile).await?;

        let name = adapter.name().to_string();
        let addr = adapter.address().await?;
        println!("Adapter {} ready at {}", name, addr);

        Ok(Self { adapter, _session: session, _profile_handle })
    }

    pub async fn address(&self) -> Result<Address> {
        Ok(self.adapter.address().await?)
    }

    pub async fn set_discoverable(&self, enabled: bool) -> Result<()> {
        self.adapter.set_discoverable(enabled).await?;
        Ok(())
    }

    pub fn set_device_class(&self) -> Result<()> {
        let name = self.adapter.name();
        let status = Command::new("hciconfig")
            .arg(name)
            .arg("class")
            .arg(GAMEPAD_CLASS)
            .status()
            .context("hciconfig failed to spawn")?;
        anyhow::ensure!(status.success(), "hciconfig class failed");
        Ok(())
    }
}

async fn remove_known_switches(adapter: &Adapter) -> Result<()> {
    let addrs = adapter.device_addresses().await?;
    let mut removed = 0;
    for addr in addrs {
        if let Ok(dev) = adapter.device(addr) {
            if let Ok(alias) = dev.alias().await {
                if alias.to_uppercase() == "NINTENDO SWITCH" {
                    let _ = adapter.remove_device(addr).await;
                    removed += 1;
                }
            }
        }
    }
    if removed > 0 {
        println!("Cleared {} stale Nintendo Switch pairing(s)", removed);
    }
    Ok(())
}

fn prepare_sdp() -> Result<()> {
    Command::new("chmod")
        .args(["777", "/var/run/sdp"])
        .status()
        .context("chmod /var/run/sdp failed")?;

    let out = Command::new("sdptool")
        .args(["browse", "local"])
        .output()
        .context("sdptool browse failed")?;
    let stdout = String::from_utf8_lossy(&out.stdout);

    let handles: Vec<String> = stdout
        .split("\n\n")
        .filter(|b| !b.contains("PnP Information"))
        .flat_map(|b| b.lines())
        .filter(|l| l.contains("Service RecHandle"))
        .filter_map(|l| l.split_whitespace().last().map(str::to_string))
        .collect();

    for h in &handles {
        let _ = Command::new("sdptool").args(["del", h]).status();
    }

    if !handles.is_empty() {
        println!("Cleared {} stale SDP record(s)", handles.len());
    }
    Ok(())
}
