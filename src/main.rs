use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use anyhow::Result;
use bluer::{Address, AddressType};
use bluer::l2cap::{SeqPacket, SeqPacketListener, SocketAddr};
use tokio::sync::watch;
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

    let pool:   Arc<Mutex<Vec<String>>>     = Arc::new(Mutex::new(names));
    let active: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

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
                                act.insert(path.clone());
                                let manager = Arc::clone(&manager);
                                let pool    = Arc::clone(&pool);
                                let active  = Arc::clone(&active);
                                tokio::task::spawn_local(async move {
                                    controller_task(manager, name.clone(), path.clone()).await;
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

async fn controller_task(manager: Arc<BluetoothManager>, adapter_name: String, drum_path: String) {
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

    loop {
        if rx.has_changed().is_err() {
            println!("[{addr_str}] drum unplugged — releasing adapter");
            break;
        }

        let quit = tokio::select! {
            _ = tokio::signal::ctrl_c() => true,
            res = run_connection(&adapter, addr, &addr_str, rx.clone()) => match res {
                Ok(())  => false,
                Err(e)  => {
                    eprintln!("[{addr_str}] {e} — retrying in 2s");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    false
                }
            }
        };

        if quit { break; }
    }
}

async fn run_connection(
    adapter:  &ControllerAdapter,
    addr:     Address,
    addr_str: &str,
    rx:       watch::Receiver<ButtonState>,
) -> Result<()> {
    // Bind sockets before set_discoverable — toggling discoverable resets device class
    let ln_ctrl = SeqPacketListener::bind(SocketAddr { addr, addr_type: AddressType::BrEdr, psm: PSM_CTRL, cid: 0 }).await?;
    let ln_itrp = SeqPacketListener::bind(SocketAddr { addr, addr_type: AddressType::BrEdr, psm: PSM_ITRP, cid: 0 }).await?;

    adapter.set_discoverable(true).await?;
    adapter.set_device_class()?;

    println!("\n[{addr_str}] Waiting — on Switch: Controllers → Change Grip/Order");

    let (mut itrp, peer) = ln_itrp.accept().await?;
    let (ctrl, _)        = ln_ctrl.accept().await?;

    println!("[{addr_str}] Switch connected from {}", peer.addr);

    run_handshake(&mut itrp, addr_str).await?;
    idle_loop(&mut itrp, ctrl, rx).await
}

async fn run_handshake(itrp: &mut SeqPacket, addr_str: &str) -> Result<()> {
    let mut hs = Handshake::new(addr_str);

    let init = hs.process(None);
    itrp.send(&init).await?;

    let mut received = false;
    while !hs.is_complete() {
        let mut buf = [0u8; 50];
        let n = match timeout(Duration::from_millis(1), itrp.recv(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => { received = true; Some(n) }
            _                  => None,
        };

        let reply = hs.process(n.map(|n| &buf[..n]));
        itrp.send(&reply).await?;

        if received {
            tokio::time::sleep(Duration::from_millis(1000 / 15)).await;
        } else {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    println!("Pairing complete — assigned player {}", hs.player_number.unwrap_or(0));
    Ok(())
}

async fn idle_loop(
    itrp: &mut SeqPacket,
    _ctrl: SeqPacket,
    rx: watch::Receiver<ButtonState>,
) -> Result<()> {
    let mut timer  = Timer::new();
    let mut ticker = interval(Duration::from_micros(7576));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    println!("Entering idle loop (Ctrl-C to quit)");

    loop {
        ticker.tick().await;

        let mut buf = [0u8; 50];
        let _ = timeout(Duration::from_micros(100), itrp.recv(&mut buf)).await;

        if rx.has_changed().is_err() {
            return Err(anyhow::anyhow!("drum unplugged"));
        }

        let state = rx.borrow().clone();
        let msg   = input_report(&mut timer, &state);
        itrp.send(&msg).await
            .map_err(|e| anyhow::anyhow!("Switch disconnected: {e}"))?;
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
