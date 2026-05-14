use std::path::{Path, PathBuf};
use std::process::{Command, exit};

const TARGET: &str = "aarch64-unknown-linux-gnu";
const DEFAULT_REMOTE_PATH: &str = "~/wireless-taiko";

fn main() {
    let task = std::env::args().nth(1);
    match task.as_deref() {
        Some("deploy") => deploy(),
        _ => {
            eprintln!("Usage: cargo deploy");
            exit(1);
        }
    }
}

fn deploy() {
    let root = workspace_root();
    let host = env_or_die("TAIKO_HOST");
    let remote_path = std::env::var("TAIKO_REMOTE_PATH")
        .unwrap_or_else(|_| DEFAULT_REMOTE_PATH.to_string());

    let sccache_dir = format!("{}/.cache/sccache", env_or_die("HOME"));
    let container_opts = format!("--volume {sccache_dir}:{sccache_dir}");

    println!("[deploy] Cross-compiling for {TARGET}...");
    run(
        Command::new("cross")
            .args(["build", "--release", "--target", TARGET])
            .env("DOCKER_DEFAULT_PLATFORM", "linux/amd64")
            .env("RUSTC_WRAPPER", "sccache")
            .env("SCCACHE_DIR", &sccache_dir)
            .env("CROSS_CONTAINER_OPTS", &container_opts)
            .current_dir(&root),
    );

    let binary = root
        .join("target")
        .join(TARGET)
        .join("release")
        .join("wireless-taiko");

    println!("[deploy] Deploying to {host}:{remote_path}...");
    run(Command::new("scp").args([binary.as_os_str(), format!("{host}:{remote_path}").as_ref()]));

    println!("[deploy] Done. Run with:  ssh {host} sudo {remote_path}");
}

fn run(cmd: &mut Command) {
    let status = cmd.status().unwrap_or_else(|e| {
        eprintln!("failed to spawn {:?}: {e}", cmd.get_program());
        exit(1);
    });
    if !status.success() {
        eprintln!("{:?} exited with {status}", cmd.get_program());
        exit(status.code().unwrap_or(1));
    }
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is xtask/ at compile time; workspace root is one level up
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has no parent directory")
        .to_owned()
}

fn env_or_die(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| {
        eprintln!("${key} not set");
        exit(1);
    })
}
