// SPDX-License-Identifier: GPL-3.0-or-later
mod bluetooth;
mod daemon;
mod media;
mod model;
mod protocol;
mod storage;

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use model::{Mode, Snapshot};
use serde::{Deserialize, Serialize};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot, watch, Semaphore},
    time::timeout,
};

#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "lowercase", deny_unknown_fields)]
enum WireCommand {
    Status,
    Watch,
    Setup { address: String },
    Mode { mode: Mode },
    Release,
}

/// Set by the systemd unit for plugin installs. Empty or unset for
/// `install.sh --backend-only` and manual runs, which have no plugin folder.
const PLUGIN_DIR_ENV: &str = "AIRPODSD_PLUGIN_DIR";
const RESPONSE_LIMIT: usize = 65536;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(40);
const WATCH_RETRY_MIN: Duration = Duration::from_secs(1);
const WATCH_RETRY_MAX: Duration = Duration::from_secs(5);

fn socket_path() -> Result<PathBuf> {
    Ok(storage::runtime_dir()?.join("control.sock"))
}

fn plugin_dir() -> Result<Option<PathBuf>> {
    let Some(dir) = std::env::var_os(PLUGIN_DIR_ENV).filter(|dir| !dir.is_empty()) else {
        return Ok(None);
    };
    let dir = PathBuf::from(dir);
    if !dir.is_absolute() {
        bail!("{PLUGIN_DIR_ENV} must be an absolute path");
    }
    Ok(Some(dir))
}

fn lock() -> Result<fs::File> {
    let file = storage::private_open(&storage::runtime_dir()?.join("daemon.lock"), true)?;
    file.try_lock_exclusive()
        .context("another airpodsd process owns the device policy")?;
    Ok(file)
}

async fn write_json(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    value: &impl Serialize,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    timeout(Duration::from_secs(5), writer.write_all(&bytes))
        .await
        .context("control client is not reading")??;
    timeout(Duration::from_secs(5), writer.flush())
        .await
        .context("control output flush timed out")??;
    Ok(())
}

async fn serve(
    stream: UnixStream,
    mut snapshots: watch::Receiver<Snapshot>,
    tx: mpsc::Sender<daemon::Request>,
) -> Result<()> {
    if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } {
        bail!("control client has different ownership");
    }
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    let count = timeout(
        Duration::from_secs(5),
        (&mut reader).take(4097).read_until(b'\n', &mut line),
    )
    .await??;
    if count == 0 || count > 4096 || line.last() != Some(&b'\n') {
        bail!("invalid control request size");
    }
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
                Err(e) => {
                    write_json(
                        &mut writer,
                        &serde_json::json!({"ok": false, "error": format!("{e:#}")}),
                    )
                    .await?
                }
            }
        }
    }
    Ok(())
}

async fn run_daemon() -> Result<()> {
    let _lock = lock()?;
    let plugin_dir = plugin_dir()?;
    let path = socket_path()?;
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
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
    let mut policy = tokio::spawn(daemon::run(requests, snapshots, shutdown, plugin_dir));
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
    let outcome = match result {
        Some(result) => result?,
        None => policy.await?,
    };
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
        Err(_) => {
            snapshot.error = Some("Invalid private configuration; daemon unavailable".into());
        }
    }
    snapshot.status = "error".into();
    if snapshot.error.is_none() {
        snapshot.error = Some(reason.into());
    }
    snapshot
}

/// Validates one daemon response line before it is forwarded verbatim.
fn check_response(line: &str) -> Result<()> {
    if line.len() > RESPONSE_LIMIT {
        bail!("daemon response exceeds protocol limit");
    }
    let response: serde_json::Value =
        serde_json::from_str(line).context("invalid daemon response")?;
    if response["ok"] == false {
        bail!("{}", response["error"].as_str().unwrap_or("command failed"));
    }
    let _: Snapshot = serde_json::from_value(response).context("invalid snapshot schema")?;
    Ok(())
}

async fn forward(line: &str) -> Result<()> {
    let mut stdout = tokio::io::stdout();
    stdout.write_all(line.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}

async fn client(command: WireCommand) -> Result<()> {
    let path = match socket_path() {
        Ok(path) => path,
        Err(e) => {
            if matches!(command, WireCommand::Status) {
                write_json(
                    &mut tokio::io::stdout(),
                    &offline_snapshot("airpodsd runtime directory is unavailable"),
                )
                .await?;
            }
            return Err(e);
        }
    };
    let stream = match UnixStream::connect(path).await {
        Ok(stream) => stream,
        Err(e) => {
            match command {
                WireCommand::Status => {
                    write_json(
                        &mut tokio::io::stdout(),
                        &offline_snapshot("airpodsd daemon is unavailable"),
                    )
                    .await?;
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
    let (reader, mut writer) = stream.into_split();
    write_json(&mut writer, &command).await?;
    let mut line = String::new();
    let count = timeout(COMMAND_TIMEOUT, BufReader::new(reader).read_line(&mut line))
        .await
        .context("daemon command timed out")??;
    if count == 0 {
        bail!("daemon control connection closed");
    }
    check_response(&line)?;
    forward(&line).await
}

/// Forwards one daemon session's snapshots until the daemon closes it.
/// Returns whether any snapshot arrived. Daemon-side I/O failures end the
/// session; invalid responses and stdout failures end the watch.
async fn relay(stream: UnixStream) -> Result<bool> {
    let (reader, mut writer) = stream.into_split();
    if write_json(&mut writer, &WireCommand::Watch).await.is_err() {
        return Ok(false);
    }
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let mut delivered = false;
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return Ok(delivered),
            Ok(_) => {}
        }
        check_response(&line)?;
        forward(&line).await?;
        delivered = true;
    }
}

/// The panel's long-lived snapshot stream. A stopped or restarting daemon
/// yields one offline snapshot per outage and a reconnect with backoff, so
/// the panel keeps one process instead of respawning it.
async fn watch() -> Result<()> {
    let path = match socket_path() {
        Ok(path) => path,
        Err(e) => {
            write_json(
                &mut tokio::io::stdout(),
                &offline_snapshot("airpodsd runtime directory is unavailable"),
            )
            .await?;
            return Err(e);
        }
    };
    let mut delay = WATCH_RETRY_MIN;
    let mut reported_offline = false;
    loop {
        let delivered = match UnixStream::connect(&path).await {
            Ok(stream) => relay(stream).await?,
            Err(_) => false,
        };
        if delivered {
            delay = WATCH_RETRY_MIN;
            reported_offline = false;
        }
        if !reported_offline {
            write_json(
                &mut tokio::io::stdout(),
                &offline_snapshot("airpodsd daemon is unavailable"),
            )
            .await?;
            reported_offline = true;
        }
        tokio::time::sleep(delay).await;
        if !delivered {
            delay = (delay * 2).min(WATCH_RETRY_MAX);
        }
    }
}

async fn execute() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["daemon"] => run_daemon().await,
        ["guard"] => { let _lock = lock()?; daemon::offline_guard(false).await },
        ["status"] => client(WireCommand::Status).await,
        ["watch"] => watch().await,
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
    if let Err(e) = execute().await {
        eprintln!("airpodsd: {e:#}");
        std::process::exit(1);
    }
}
