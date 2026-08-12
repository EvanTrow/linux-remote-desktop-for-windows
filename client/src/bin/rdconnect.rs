//! Single-command launcher for the full multi-monitor session: detects this machine's connected
//! outputs via `xrandr`, matches them to the host's known topology (`topology.rs`), pairs with
//! any Sunshine instance not already paired (one-time PIN entry per instance, same as any
//! Moonlight client — see `pairing.rs`), then spawns one `rdclient` child process per monitor
//! with the right `--port`/`--width`/`--height`/`--output` already filled in.
//!
//! Exists because `moonlight-common-c` is a single-global-connection-per-process library (see
//! PLAN.md's multi-monitor architecture notes) — one process genuinely can't stream more than one
//! monitor, so "one command, no manual per-monitor flags" has to mean *orchestrating* several
//! `rdclient` processes rather than replacing them.

use anyhow::{bail, Context, Result};
use clap::Parser;
use rdclient::{pairing, topology};
use std::io::{BufRead, Write};

#[derive(Parser, Debug)]
struct Args {
    /// Sunshine host to connect to (bare hostname/IP, no port).
    host: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let identity = pairing::ClientIdentity::load_or_generate()?;

    let targets = topology::target_topology();
    let outputs = topology::detect_local_outputs().context("detecting local monitor layout via xrandr")?;
    let matched = topology::match_targets_to_outputs(&targets, &outputs)
        .context("matching this machine's connected outputs to the host's expected topology")?;

    for (target, _output_name) in &matched {
        if pairing::pair_status(&identity, &args.host, target.sunshine_port).await.unwrap_or(false) {
            println!("{} (port {}) already paired", target.label, target.sunshine_port);
            continue;
        }

        print!("{} (port {}) not yet paired — pick a PIN (any digits) and press enter: ", target.label, target.sunshine_port);
        std::io::stdout().flush()?;
        let mut pin = String::new();
        std::io::stdin().lock().read_line(&mut pin)?;
        let pin = pin.trim();
        if pin.is_empty() {
            bail!("no PIN entered for {}", target.label);
        }

        println!(
            "waiting for that same PIN to be entered at https://{}:{} (PIN section) ...",
            args.host,
            target.sunshine_port + 1
        );
        pairing::pair(&identity, &args.host, target.sunshine_port, pin).await.with_context(|| format!("pairing with {}", target.label))?;
        println!("{} paired", target.label);
    }

    let rdclient_path = std::env::current_exe()
        .context("locating this executable")?
        .parent()
        .context("executable has no parent directory")?
        .join("rdclient");
    if !rdclient_path.exists() {
        bail!("expected rdclient built alongside rdconnect at {} — build both with `cargo build --release`", rdclient_path.display());
    }

    let mut children = Vec::new();
    for (target, output_name) in &matched {
        println!("launching {} on {} (port {})", target.label, output_name, target.sunshine_port);
        let child = std::process::Command::new(&rdclient_path)
            .args([
                "--host",
                &args.host,
                "--port",
                &target.sunshine_port.to_string(),
                "--width",
                &target.width.to_string(),
                "--height",
                &target.height.to_string(),
                "--output",
                output_name,
                "--capture-input",
            ])
            .spawn()
            .with_context(|| format!("launching rdclient for {}", target.label))?;
        children.push(child);
    }

    // Forward Ctrl+C to every child rather than just killing this launcher — each child's own
    // signal handler (main.rs) calls LiStopConnection() before exiting, so the host's app session
    // ends cleanly instead of getting stuck "already running" for the next connect.
    let child_pids: Vec<u32> = children.iter().map(|c| c.id()).collect();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        for pid in &child_pids {
            let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
        }
    });

    // Block until every child exits (normal quit, or killed above) — keeps this launcher (and
    // the Ctrl+C forwarding task) alive for the life of the session instead of exiting the moment
    // all 3 have been spawned.
    tokio::task::spawn_blocking(move || {
        for mut child in children {
            let _ = child.wait();
        }
    })
    .await
    .context("waiting on rdclient child processes")?;

    Ok(())
}
