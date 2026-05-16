// Entry point and top-level orchestration.
//
// Each USB Taiko drum gets its own Bluetooth adapter and presents to the Switch
// as an independent Pro Controller. The adapter pool (Vec<String>) tracks which
// adapters are free; the active set (HashSet<String>) tracks which drum paths
// already have running tasks so we don't spawn duplicates on the next poll.
//
// Runtime: single-threaded tokio (`current_thread`) + LocalSet. bluer's Adapter
// type is not Send (it holds Rc-based D-Bus handles), so we use spawn_local
// instead of spawn — spawn_local requires a LocalSet on the current-thread runtime.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use anyhow::Result;
use bluer::AddressType;
use bluer::l2cap::{SeqPacket, SeqPacketListener, SocketAddr};
use tokio::sync::{watch, Semaphore};
use tokio::time::{interval, timeout};

mod bluetooth;
mod protocol;
mod usb;

use bluetooth::{BluetoothManager, ControllerAdapter};
use protocol::{ButtonState, Handshake, Timer, input_report};

// HID Control (PSM 17) and HID Interrupt (PSM 19) are the two L2CAP channels
// in the HID-over-Bluetooth profile. The Switch sends all subcommands on PSM 19
// (interrupt), not PSM 17 (control) — PSM 17 is bound to satisfy the profile
// requirement but carries no traffic in practice.
const PSM_CTRL: u16 = 17;
const PSM_ITRP: u16 = 19;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    check_root();

    // BlueZ compatibility mode (-C) creates /var/run/sdp for legacy SDP. If
    // it's missing, bluetoothd was started without -C and profile registration
    // will silently fail (the Switch won't see a HID profile during SDP browse).
    if !std::path::Path::new("/var/run/sdp").exists() {
        eprintln!(
            "Error: /var/run/sdp missing\n\
             BlueZ must run in compatibility mode:\n\
               bluetoothd -C --noplugin=input,sap,avrcp"
        );
        std::process::exit(1);
    }

    // One shared BlueZ session for all adapters — BlueZ only allows one SDP
    // profile registration per session (process).
    let manager = Arc::new(BluetoothManager::new().await?);

    let names = manager.adapter_names().await?;
    if names.is_empty() {
        eprintln!("Error: no Bluetooth adapters found");
        std::process::exit(1);
    }
    println!("Found {} adapter(s): {}", names.len(), names.join(", "));

    let pool:        Arc<Mutex<Vec<String>>>     = Arc::new(Mutex::new(names));
    let active:      Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    // The pairing semaphore serializes initial pairing across adapters. If two
    // drums are plugged in simultaneously, both adapters would advertise as Pro
    // Controllers and the Switch CGO screen would show two controllers at once —
    // making it ambiguous which to pair. With the semaphore, only one adapter
    // holds the permit (and thus advertises) at a time during first pairing.
    // Reconnects don't need the permit — the Switch connects directly to the
    // known MAC without the CGO screen.
    let pairing_lock: Arc<Semaphore>             = Arc::new(Semaphore::new(1));

    // spawn_local requires a LocalSet when using current_thread runtime
    let local = tokio::task::LocalSet::new();
    local.run_until(async {
        let mut ticker = interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    println!("\nExiting");
                    break;
                }
                _ = ticker.tick() => {
                    // Poll for connected drums every second. hidapi re-enumerates
                    // on each call, so newly plugged drums are detected within ~1s.
                    let drums = usb::find_drums();
                    let mut act = active.lock().unwrap();
                    for path in drums {
                        if act.contains(&path) { continue; }
                        let adapter_name = pool.lock().unwrap().pop();
                        match adapter_name {
                            None => eprintln!("[main] no free adapter for drum at {path}"),
                            Some(name) => {
                                println!("[main] drum found at {path}, assigning adapter {name}");
                                act.insert(path.clone());
                                let manager      = Arc::clone(&manager);
                                let pool         = Arc::clone(&pool);
                                let active       = Arc::clone(&active);
                                let pairing_lock = Arc::clone(&pairing_lock);
                                tokio::task::spawn_local(async move {
                                    controller_task(manager, name.clone(), path.clone(), pairing_lock).await;
                                    // Return the adapter and drum path to the pool
                                    // so they can be reused for the next plug-in.
                                    pool.lock().unwrap().push(name);
                                    active.lock().unwrap().remove(&path);
                                });
                            }
                        }
                    }
                }
            }
        }
    }).await;

    Ok(())
}

async fn controller_task(manager: Arc<BluetoothManager>, adapter_name: String, drum_path: String, pairing_lock: Arc<Semaphore>) {
    let adapter = match manager.setup_adapter(&adapter_name).await {
        Ok(a)  => a,
        Err(e) => { eprintln!("[{adapter_name}] setup failed: {e}"); return; }
    };
    let addr = match adapter.address().await {
        Ok(a)  => a,
        Err(e) => { eprintln!("[{adapter_name}] address error: {e}"); return; }
    };
    let addr_str = addr.to_string();

    // ButtonState is sent from the blocking USB reader thread to this async task
    // via a watch channel. watch delivers only the latest value, discarding
    // intermediate states — correct since we only care about the current drum
    // state at each 132 Hz report tick.
    let (tx, rx) = watch::channel(ButtonState::default());
    let drum_path_clone = drum_path.clone();
    tokio::task::spawn_blocking(move || usb::read_drum(drum_path_clone, tx));

    // Bind L2CAP listeners once for the lifetime of this task. Keeping them
    // alive means the kernel queues Switch reconnection attempts even while
    // we're between sessions (no rebind delay, no missed reconnects during the
    // clear_switch_pairing window).
    let ln_ctrl = match SeqPacketListener::bind(SocketAddr { addr, addr_type: AddressType::BrEdr, psm: PSM_CTRL, cid: 0 }).await {
        Ok(l)  => l,
        Err(e) => { eprintln!("[{addr_str}] bind failed: {e}"); return; }
    };
    let ln_itrp = match SeqPacketListener::bind(SocketAddr { addr, addr_type: AddressType::BrEdr, psm: PSM_ITRP, cid: 0 }).await {
        Ok(l)  => l,
        Err(e) => { eprintln!("[{addr_str}] bind failed: {e}"); return; }
    };

    // After the first successful pairing the Switch knows our MAC and reconnects
    // directly — no advertising needed, no semaphore held for reconnects.
    let ever_paired = Arc::new(AtomicBool::new(false));

    loop {
        if rx.has_changed().is_err() {
            println!("[{addr_str}] drum unplugged — releasing adapter");
            break;
        }

        let quit = tokio::select! {
            _ = tokio::signal::ctrl_c() => true,
            res = run_connection(&adapter, &ln_ctrl, &ln_itrp, &addr_str, rx.clone(), Arc::clone(&pairing_lock), Arc::clone(&ever_paired)) => match res {
                Ok(())  => false,
                Err(e)  => {
                    eprintln!("[{addr_str}] {e} — retrying");
                    false
                }
            }
        };

        if quit { break; }
    }
}

async fn run_connection(
    adapter:      &ControllerAdapter,
    ln_ctrl:      &SeqPacketListener,
    ln_itrp:      &SeqPacketListener,
    addr_str:     &str,
    rx:           watch::Receiver<ButtonState>,
    pairing_lock: Arc<Semaphore>,
    ever_paired:  Arc<AtomicBool>,
) -> Result<()> {
    // Remove any stale Switch pairing so BlueZ treats this as a new device and
    // accepts Just Works SSP without prompting. The noscan trick inside
    // clear_switch_pairing prevents the Switch from reconnecting mid-removal
    // (which would cause remove_device to fail silently, leaving the link key).
    adapter.clear_switch_pairing().await;

    let (mut itrp, permit) = if !ever_paired.load(Ordering::Relaxed) {
        // Initial pairing: hold the semaphore so only one adapter is visible in
        // the CGO "Change Grip/Order" screen at a time.
        let permit = pairing_lock.acquire_owned().await?;
        adapter.set_discoverable(true).await?;
        // set_device_class must come after set_discoverable — BlueZ resets the
        // CoD when toggling discoverable, so setting it first would be lost.
        adapter.set_device_class()?;
        println!("\n[{addr_str}] Waiting — on Switch: Controllers → Change Grip/Order");
        let (itrp, peer) = ln_itrp.accept().await?;
        println!("[{addr_str}] Switch connected from {}", peer.addr);
        ever_paired.store(true, Ordering::Relaxed);
        (itrp, Some(permit))
    } else {
        // Reconnect: the Switch pages us directly using our MAC.
        adapter.set_discoverable(true).await?;
        adapter.set_device_class()?;
        println!("[{addr_str}] Waiting for Switch to reconnect...");
        let (itrp, peer) = ln_itrp.accept().await?;
        println!("[{addr_str}] Switch reconnected from {}", peer.addr);
        (itrp, None)
    };

    // PSM 17 (control) must also accept — the Switch expects both channels.
    let (ctrl, _) = ln_ctrl.accept().await?;
    run_loop(&mut itrp, ctrl, rx, addr_str, permit).await
}

// Unified 132 Hz loop handling both handshake and idle phases.
//
// The Switch polls at ~8ms (125 Hz). We run slightly faster at 132 Hz
// (7576μs/tick) so we're always ready to reply before the Switch times out.
//
// Two send modes:
//   Subcommand reply (0xA2 from Switch): always sent immediately as a 0x21
//     subcommand-reply report, no throttling.
//   Input report (no subcommand): sent as a 0x30 standard input report only
//     when buttons change or once per throttle window (idle keepalive).
//
// The throttle avoids flooding the BT send buffer. During the handshake phase,
// sending an input report on every tick fills the buffer faster than BlueZ can
// drain it, which blocks itrp.send() for 200+ ms and causes the Switch to time
// out waiting for subcommand replies. After the handshake we relax to ~60 Hz
// (every 2 ticks) to keep the connection alive without starving subcommands.
async fn run_loop(
    itrp:           &mut SeqPacket,
    _ctrl:          SeqPacket,
    rx:             watch::Receiver<ButtonState>,
    addr_str:       &str,
    pairing_permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> Result<()> {
    let mut timer        = Timer::new();
    let mut hs           = Handshake::new(addr_str);
    let mut permit       = pairing_permit;
    let mut ticker       = interval(Duration::from_micros(7576)); // ~132 Hz
    let mut idle_ticks:  u32        = 0;
    let mut last_buttons = ButtonState::default();
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // The Switch expects an unsolicited input report immediately on connect
    // (before the first subcommand) to confirm the connection is live.
    let init = hs.process(None);
    itrp.send(&init).await
        .map_err(|e| anyhow::anyhow!("Switch disconnected: {e}"))?;

    loop {
        ticker.tick().await;
        idle_ticks = idle_ticks.wrapping_add(1);

        let mut buf = [0u8; 50];
        // 100μs recv window: short enough to keep within the 7576μs tick budget,
        // long enough to capture data that arrives on this tick.
        let n = match timeout(Duration::from_micros(100), itrp.recv(&mut buf)).await {
            Ok(Ok(0))  => return Err(anyhow::anyhow!("Switch disconnected")),
            Ok(Ok(n))  => n,
            Ok(Err(e)) => return Err(anyhow::anyhow!("Switch disconnected: {e}")),
            Err(_)     => 0, // timeout — no data this tick
        };

        // If tx dropped, the USB reader exited (drum unplugged).
        if rx.has_changed().is_err() {
            return Err(anyhow::anyhow!("drum unplugged"));
        }

        if n > 0 && buf[0] == 0xA2 {
            // 0xA2 = HID SET_REPORT (Switch → us). Always reply immediately.
            let reply = hs.process(Some(&buf[..n]));
            if permit.is_some() && hs.is_complete() {
                println!("Pairing complete — assigned player {}", hs.player_number.unwrap_or(0));
                // Drop the permit to release the semaphore so the next adapter
                // can begin advertising in CGO.
                drop(permit.take());
            }
            itrp.send(&reply).await
                .map_err(|e| anyhow::anyhow!("Switch disconnected: {e}"))?;
        } else {
            let state = rx.borrow().clone();
            // During handshake: send input reports rarely (every 132 ticks ≈ 1s)
            // to avoid filling the BT send buffer and delaying subcommand replies.
            // After handshake: send at ~60 Hz (every 2 ticks) for responsiveness.
            let threshold = if hs.is_complete() { 2 } else { 132 };
            if state != last_buttons || idle_ticks >= threshold {
                let msg = input_report(&mut timer, &state);
                itrp.send(&msg).await
                    .map_err(|e| anyhow::anyhow!("Switch disconnected: {e}"))?;
                last_buttons = state;
                idle_ticks = 0;
            }
        }
    }
}

fn check_root() {
    let uid = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse::<u32>().ok())
        })
        .unwrap_or(1);

    if uid != 0 {
        eprintln!("Error: must run as root (sudo ./wireless-taiko)");
        std::process::exit(1);
    }
}
