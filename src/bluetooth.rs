// Bluetooth setup for Pro Controller emulation.
//
// BlueZ (the Linux Bluetooth stack) exposes its API over D-Bus. bluer is the
// Rust client. One Session = one D-Bus connection to BlueZ. BlueZ limits each
// session to one SDP profile registration, so BluetoothManager is created once
// at startup and shared across all adapter tasks via Arc.
//
// Two-struct split:
//   BluetoothManager  — owns the Session, AgentHandle, and ProfileHandle.
//                       Dropping any of these unregisters them from BlueZ.
//   ControllerAdapter — wraps a single Adapter for per-connection operations.
//                       Can be created/destroyed per-session without affecting
//                       the shared session or profile.

use std::process::Command;
use std::time::Duration;
use anyhow::{Context, Result};
use bluer::{Address, Adapter, Session};
use bluer::agent::{Agent, AgentHandle};
use bluer::rfcomm::{Profile, ProfileHandle};

// 00001000-…-00805f9b34fb is the Bluetooth "Browse Group" root UUID.
// Registering our SDP record under this UUID ensures the Switch can find the
// HID profile when it browses the device's service list before connecting.
const SDP_UUID: &str = "00001000-0000-1000-8000-00805f9b34fb";

// CoD (Class of Device) for Peripheral/Gamepad (0x002508).
// The Switch checks the CoD to confirm it's talking to a game controller.
// bluer cannot set this — we call hciconfig directly.
const GAMEPAD_CLASS: &str = "0x002508";

pub struct BluetoothManager {
    session:         Session,
    // Dropping _agent_handle unregisters the agent from BlueZ.
    _agent_handle:   AgentHandle,
    // Dropping _profile_handle removes the SDP HID record from BlueZ.
    _profile_handle: ProfileHandle,
}

impl BluetoothManager {
    pub async fn new() -> Result<Self> {
        let session = Session::new().await?;
        prepare_sdp()?;

        // The Bluetooth SSP (Secure Simple Pairing) IO capability we advertise
        // determines which pairing method BlueZ uses:
        //   DisplayYesNo → Numeric Comparison  (requires user confirmation)
        //   NoInputNoOutput → Just Works        (auto-confirmed, no interaction)
        //
        // If we register any agent callback (e.g. request_confirmation), bluer
        // advertises DisplayYesNo, which leads to Numeric Comparison. BlueZ
        // auto-rejects Numeric Comparison for already-known devices (ones with
        // a stored link key) in ~165μs — too fast for the D-Bus agent callback
        // to respond. Registering only `request_default: true` signals
        // NoInputNoOutput → Just Works → auto-confirmed on both sides.
        let _agent_handle = session.register_agent(Agent {
            request_default: true,
            ..Default::default()
        }).await?;

        // The SDP (Service Discovery Protocol) record describes our HID profile
        // to the Switch. The Switch performs an SDP browse before connecting to
        // verify the remote device supports HID. sdp_record.xml is a standard
        // Pro Controller HID descriptor — the Switch checks specific fields
        // (SubClass, DescriptorList) to confirm the device type.
        let profile = Profile {
            uuid: SDP_UUID.parse().context("invalid SDP UUID")?,
            service_record: Some(include_str!("../sdp_record.xml").to_string()),
            require_authentication: Some(false),
            require_authorization: Some(false),
            auto_connect: Some(true),
            ..Default::default()
        };
        let _profile_handle = session.register_profile(profile).await?;

        Ok(Self { session, _agent_handle, _profile_handle })
    }

    pub async fn adapter_names(&self) -> Result<Vec<String>> {
        Ok(self.session.adapter_names().await?)
    }

    pub async fn setup_adapter(&self, name: &str) -> Result<ControllerAdapter> {
        let adapter = self.session.adapter(name)?;

        // Remove any Switch pairings cached from a previous run. At startup
        // the Switch won't be mid-connection, so a plain remove (no noscan
        // trick) is safe here.
        remove_known_switches(&adapter).await?;

        adapter.set_powered(true).await?;
        adapter.set_pairable(true).await?;
        // timeout=0 means infinite — any finite timeout would cause BlueZ to
        // stop accepting new pairings before the user opens CGO.
        adapter.set_pairable_timeout(0).await?;
        adapter.set_discoverable_timeout(0).await?;
        // The Switch displays this name in the Controllers menu. It must say
        // "Pro Controller" for the Switch to treat us as a Pro Controller.
        adapter.set_alias("Pro Controller".to_string()).await?;

        let addr = adapter.address().await?;
        println!("Adapter {name} ready at {addr}");

        Ok(ControllerAdapter { adapter })
    }
}

pub struct ControllerAdapter {
    pub adapter: Adapter,
}

impl ControllerAdapter {
    pub async fn address(&self) -> Result<Address> {
        Ok(self.adapter.address().await?)
    }

    // set_discoverable enables/disables inquiry scan (ISCAN). The Switch
    // needs to find us during initial pairing; for reconnects it pages us
    // directly using our known MAC, so ISCAN is less critical then.
    pub async fn set_discoverable(&self, enabled: bool) -> Result<()> {
        self.adapter.set_discoverable(enabled).await?;
        Ok(())
    }

    // bluer has no API to set the CoD, so we shell out to hciconfig.
    // Must be called AFTER set_discoverable — BlueZ resets the CoD when
    // toggling discoverable, so setting it first would be overwritten.
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

    /// Remove any known Nintendo Switch pairings so BlueZ treats the next
    /// connection as a new device, enabling Just Works SSP auto-accept.
    ///
    /// The noscan dance is required because the Switch continuously pages us
    /// if it has our MAC in its pairing list (even while we're in the CGO
    /// screen). If remove_device is called while the Switch has an active ACL
    /// connection, BlueZ silently fails and the stale link key remains — on
    /// the next SSP attempt BlueZ sees a known device, escalates to Numeric
    /// Comparison, and rejects it. Disabling page scan (noscan) prevents the
    /// Switch from reconnecting during the removal window.
    pub async fn clear_switch_pairing(&self) {
        let name = self.adapter.name();

        // PSCAN = page scan (connectable). Disabling it gives us a clean
        // window to remove the device without a race against the Switch paging.
        let _ = Command::new("hciconfig").args([name, "noscan"]).status();
        tokio::time::sleep(Duration::from_millis(200)).await;

        if let Ok(addrs) = self.adapter.device_addresses().await {
            for addr in addrs {
                if let Ok(dev) = self.adapter.device(addr) {
                    if dev.alias().await.map(|a| a.to_uppercase() == "NINTENDO SWITCH").unwrap_or(false) {
                        // disconnect() drops any active ACL link before removal.
                        let _ = dev.disconnect().await;
                        let _ = self.adapter.remove_device(addr).await;
                    }
                }
            }
        }

        // Restore page scan so the Switch can connect once we advertise.
        let _ = Command::new("hciconfig").args([name, "pscan"]).status();
    }
}

// Clears stale Switch pairing records at startup (no noscan needed since the
// Switch won't be mid-connection to us yet).
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
        println!("Cleared {removed} stale Nintendo Switch pairing(s)");
    }
    Ok(())
}

// Prepares the BlueZ SDP daemon for a fresh profile registration.
//
// BlueZ in compatibility mode (-C) runs a legacy SDP daemon and exposes it via
// a Unix socket at /var/run/sdp. sdptool communicates with that socket. The
// socket is typically owned by root; chmod 777 lets sdptool work without sudo
// when the process is already root.
//
// Stale SDP records from a previous crash or run accumulate in BlueZ's memory
// (they're not cleaned up automatically until the registering process exits
// cleanly). We delete them here so the Switch sees exactly one HID profile.
// PnP Information records are kept — they're added by BlueZ itself and don't
// interfere with HID discovery.
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
