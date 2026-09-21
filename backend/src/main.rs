// SPDX-License-Identifier: AGPL-3.0-only
mod bluetooth;
mod daemon;
mod model;
mod protocol;
mod storage;

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{fs, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
use tokio::{io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader}, net::{UnixListener, UnixStream}, sync::{mpsc, oneshot, watch, Semaphore}, time::timeout};
use model::{Mode, Snapshot};

#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "lowercase", deny_unknown_fields)]
enum WireCommand { Status, Watch, Setup { address: String }, Mode { mode: Mode }, Release }

fn lock() -> Result<fs::File> {
    let file = storage::private_open(&storage::runtime_dir()?.join("daemon.lock"), true)?;
    file.try_lock_exclusive().context("another airpodsd process owns the device policy")?;
    Ok(file)
}

async fn write_json(writer: &mut (impl tokio::io::AsyncWrite + Unpin), value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    timeout(Duration::from_secs(5), writer.write_all(&bytes)).await.context("control client is not reading")??;
    timeout(Duration::from_secs(5), writer.flush()).await.context("control output flush timed out")??;
    Ok(())
}

async fn serve(stream: UnixStream, mut snapshots: watch::Receiver<Snapshot>, tx: mpsc::Sender<daemon::Request>) -> Result<()> {
    if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } { bail!("control client has different ownership"); }
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    let count = timeout(Duration::from_secs(5), (&mut reader).take(4097).read_until(b'\n', &mut line)).await??;
    if count == 0 || count > 4096 || line.last() != Some(&b'\n') { bail!("invalid control request size"); }
    let command: WireCommand = serde_json::from_slice(&line).context("invalid control request")?;
    match command {
        WireCommand::Status => {
            let snapshot = snapshots.borrow().clone();
            write_json(&mut writer, &snapshot).await?;
        }
        WireCommand::Watch => {
            let snapshot = snapshots.borrow_and_update().clone();
            write_json(&mut writer, &snapshot).await?;
            let mut disconnected = [0];
            loop {
                tokio::select! {
                    changed = snapshots.changed() => {
                        changed?;
                        let snapshot = snapshots.borrow_and_update().clone();
                        write_json(&mut writer, &snapshot).await?;
                    }
                    _ = reader.read(&mut disconnected) => break,
                }
            }
        }
        command => {
            let command = match command {
                WireCommand::Setup { address } => daemon::Command::Setup(address),
                WireCommand::Mode { mode } => daemon::Command::Mode(mode),
                WireCommand::Release => daemon::Command::Release,
                _ => unreachable!(),
            };
            let (reply, response) = oneshot::channel();
            tx.send(daemon::Request { command, reply }).await?;
            match response.await? {
                Ok(snapshot) => write_json(&mut writer, &snapshot).await?,
                Err(e) => write_json(&mut writer, &serde_json::json!({"ok": false, "error": format!("{e:#}")})).await?,
            }
        }
    }
    Ok(())
}

async fn run_daemon() -> Result<()> {
    let _lock = lock()?;
    let path = storage::runtime_dir()?.join("control.sock");
    match fs::remove_file(&path) { Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}, Err(e) => return Err(e.into()) }
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let (tx, requests) = mpsc::channel(16);
    let mut initial = Snapshot::default();
    if let Some(config) = storage::load()? {
        initial.configured = config.configured();
        initial.name = config.name;
        if initial.configured {
            initial.status = "error".into();
            initial.error = Some("Initializing Bluetooth connection guard".into());
        }
    }
    let (snapshots, receiver) = watch::channel(initial);
    let (stop, shutdown) = oneshot::channel();
    let mut policy = tokio::spawn(daemon::run(requests, snapshots, shutdown));
    let slots = Arc::new(Semaphore::new(16));
    let mut clients = tokio::task::JoinSet::new();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let result = loop {
        tokio::select! {
            _ = terminate.recv() => break None,
            _ = interrupt.recv() => break None,
            result = &mut policy => break Some(result),
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let Ok(permit) = slots.clone().try_acquire_owned() else { continue; };
                let receiver = receiver.clone();
                let tx = tx.clone();
                clients.spawn(async move {
                    let _permit = permit;
                    // Requests contain only public values. Protocol errors are
                    // private to this client and never dump input to logs.
                    let _ = serve(stream, receiver, tx).await;
                });
            }
            Some(_) = clients.join_next(), if !clients.is_empty() => {},
        }
    };
    let _ = stop.send(());
    clients.abort_all();
    let outcome = match result { Some(result) => result?, None => policy.await? };
    let _ = fs::remove_file(&path);
    outcome
}

fn offline_snapshot(reason: &str) -> Snapshot {
    let mut snapshot = Snapshot::default();
    match storage::load() {
        Ok(Some(config)) => {
            snapshot.configured = config.configured();
            snapshot.name = config.name;
        }
        Ok(None) => {}
        Err(_) => { snapshot.error = Some("Invalid private configuration; daemon unavailable".into()); }
    }
    snapshot.status = "error".into();
    if snapshot.error.is_none() { snapshot.error = Some(reason.into()); }
    snapshot
}

async fn client(command: WireCommand) -> Result<()> {
    let path = match storage::runtime_dir() {
        Ok(directory) => directory.join("control.sock"),
        Err(e) => {
            if matches!(command, WireCommand::Status | WireCommand::Watch) {
                write_json(&mut tokio::io::stdout(), &offline_snapshot("airpodsd runtime directory is unavailable")).await?;
            }
            return Err(e);
        }
    };
    let stream = match UnixStream::connect(path).await {
        Ok(stream) => stream,
        Err(e) => {
            match command {
                WireCommand::Status | WireCommand::Watch => {
                    write_json(&mut tokio::io::stdout(), &offline_snapshot("airpodsd daemon is unavailable")).await?;
                }
                WireCommand::Release => {
                    let _lock = lock()?;
                    daemon::offline_guard(true).await?;
                    let snapshot = Snapshot::default();
                    write_json(&mut tokio::io::stdout(), &snapshot).await?;
                    return Ok(());
                }
                _ => {}
            }
            return Err(e).context("airpodsd daemon is unavailable");
        }
    };
    let watching = matches!(command, WireCommand::Watch);
    let (reader, mut writer) = stream.into_split();
    write_json(&mut writer, &command).await?;
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        let count = if watching { reader.read_line(&mut line).await? }
            else { timeout(Duration::from_secs(40), reader.read_line(&mut line)).await.context("daemon command timed out")?? };
        if count == 0 { bail!("daemon control connection closed"); }
        if line.len() > 65536 { bail!("daemon response exceeds protocol limit"); }
        let response: serde_json::Value = serde_json::from_str(&line).context("invalid daemon response")?;
        if response["ok"] == false { bail!("{}", response["error"].as_str().unwrap_or("command failed")); }
        let _: Snapshot = serde_json::from_value(response).context("invalid snapshot schema")?;
        tokio::io::stdout().write_all(line.as_bytes()).await?;
        tokio::io::stdout().flush().await?;
        if !watching { break; }
    }
    Ok(())
}

async fn execute() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["daemon"] => run_daemon().await,
        ["guard"] => { let _lock = lock()?; daemon::offline_guard(false).await },
        ["status"] => client(WireCommand::Status).await,
        ["watch"] => client(WireCommand::Watch).await,
        ["release"] => client(WireCommand::Release).await,
        ["setup", address] => {
            let address: bluer::Address = address.parse().context("invalid Bluetooth address")?;
            client(WireCommand::Setup { address: address.to_string() }).await
        }
        ["mode", mode] => {
            let mode = match *mode { "off" => Mode::Off, "anc" => Mode::Anc, "transparency" => Mode::Transparency, "adaptive" => Mode::Adaptive, _ => bail!("mode must be off, anc, transparency, or adaptive") };
            client(WireCommand::Mode { mode }).await
        }
        ["--help"] | ["-h"] => {
            println!("airpodsd daemon|watch|status|setup <MAC>|mode <off|anc|transparency|adaptive>|release\nSetup is the explicit one-time connection exception. Release restores the device's original Blocked setting.\nThe internal guard command reapplies the owned block after a service failure.");
            Ok(())
        }
        _ => bail!("usage: airpodsd daemon|watch|status|setup <MAC>|mode <off|anc|transparency|adaptive>|release"),
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = execute().await { eprintln!("airpodsd: {e:#}"); std::process::exit(1); }
}
