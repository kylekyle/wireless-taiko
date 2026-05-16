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

const PSM_CTRL: u16 = 17;
const PSM_ITRP: u16 = 19;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    check_root();

    if !std::path::Path::new("/var/run/sdp").exists() {
        eprintln!(
            "Error: /var/run/sdp missing\n\
             BlueZ must run in compatibility mode:\n\
               bluetoothd -C --noplugin=input,sap,avrcp"
        );
        std::process::exit(1);
    }

    let manager = Arc::new(BluetoothManager::new().await?);

    let names = manager.adapter_names().await?;
    if names.is_empty() {
        eprintln!("Error: no Bluetooth adapters found");
        std::process::exit(1);
    }
    println!("Found {} adapter(s): {}", names.len(), names.join(", "));

    let pool:        Arc<Mutex<Vec<String>>>     = Arc::new(Mutex::new(names));
    let active:      Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
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

    let (tx, rx) = watch::channel(ButtonState::default());
    let drum_path_clone = drum_path.clone();
    tokio::task::spawn_blocking(move || usb::read_drum(drum_path_clone, tx));

    // Bind listeners once for the lifetime of this task. Keeping them alive means
    // the kernel queues reconnection attempts from the Switch even while we're
    // between sessions (no rebind delay, no missed reconnects).
    let ln_ctrl = match SeqPacketListener::bind(SocketAddr { addr, addr_type: AddressType::BrEdr, psm: PSM_CTRL, cid: 0 }).await {
        Ok(l)  => l,
        Err(e) => { eprintln!("[{addr_str}] bind failed: {e}"); return; }
    };
    let ln_itrp = match SeqPacketListener::bind(SocketAddr { addr, addr_type: AddressType::BrEdr, psm: PSM_ITRP, cid: 0 }).await {
        Ok(l)  => l,
        Err(e) => { eprintln!("[{addr_str}] bind failed: {e}"); return; }
    };

    // After the first successful connection the Switch knows our MAC and can
    // reconnect directly — no advertising needed for subsequent connections.
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
    // Disable page scan, remove any stale Switch pairing, re-enable page scan.
    // This ensures remove_device succeeds — if the Switch is mid-connection
    // the call fails silently, leaving a link key that causes BlueZ to treat
    // the subsequent Just Works SSP as a re-pair and auto-reject it.
    adapter.clear_switch_pairing().await;

    let (mut itrp, permit) = if !ever_paired.load(Ordering::Relaxed) {
        // Initial pairing: advertise (serialized so the Switch sees only one
        // Pro Controller at a time in the CGO screen).
        let permit = pairing_lock.acquire_owned().await?;
        adapter.set_discoverable(true).await?;
        adapter.set_device_class()?;
        println!("\n[{addr_str}] Waiting — on Switch: Controllers → Change Grip/Order");
        let (itrp, peer) = ln_itrp.accept().await?;
        println!("[{addr_str}] Switch connected from {}", peer.addr);
        ever_paired.store(true, Ordering::Relaxed);
        (itrp, Some(permit))
    } else {
        // Reconnect: Switch connects directly to our known MAC — no advertising.
        adapter.set_discoverable(true).await?;
        adapter.set_device_class()?;
        println!("[{addr_str}] Waiting for Switch to reconnect...");
        let (itrp, peer) = ln_itrp.accept().await?;
        println!("[{addr_str}] Switch reconnected from {}", peer.addr);
        (itrp, None)
    };

    let (ctrl, _) = ln_ctrl.accept().await?;
    run_loop(&mut itrp, ctrl, rx, addr_str, permit).await
}

// Unified 132 Hz loop handling both handshake and idle phases.
// Subcommand replies are always sent immediately. Idle/input reports are
// throttled — only sent when buttons change or once per ~1 s (132 ticks).
// Sending idle reports on every tick floods the BT send buffer, which blocks
// the task for 200+ ms per send and causes the Switch to time out during the
// handshake before replies arrive.
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
    let mut ticker       = interval(Duration::from_micros(7576));
    let mut idle_ticks:  u32        = 0;
    let mut last_buttons = ButtonState::default();
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let init = hs.process(None);
    itrp.send(&init).await
        .map_err(|e| anyhow::anyhow!("Switch disconnected: {e}"))?;

    loop {
        ticker.tick().await;
        idle_ticks = idle_ticks.wrapping_add(1);

        let mut buf = [0u8; 50];
        let n = match timeout(Duration::from_micros(100), itrp.recv(&mut buf)).await {
            Ok(Ok(0))  => return Err(anyhow::anyhow!("Switch disconnected")),
            Ok(Ok(n))  => n,
            Ok(Err(e)) => return Err(anyhow::anyhow!("Switch disconnected: {e}")),
            Err(_)     => 0,
        };

        if rx.has_changed().is_err() {
            return Err(anyhow::anyhow!("drum unplugged"));
        }

        if n > 0 && buf[0] == 0xA2 {
            let reply = hs.process(Some(&buf[..n]));
            if permit.is_some() && hs.is_complete() {
                println!("Pairing complete — assigned player {}", hs.player_number.unwrap_or(0));
                drop(permit.take());
            }
            itrp.send(&reply).await
                .map_err(|e| anyhow::anyhow!("Switch disconnected: {e}"))?;
        } else {
            let state = rx.borrow().clone();
            // Throttle during handshake to avoid flooding BT buffer (which would
            // delay subcommand replies and cause the Switch to time out).
            // After handshake, send at ~60 Hz to keep the Switch connection alive.
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
