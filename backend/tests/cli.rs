// SPDX-License-Identifier: GPL-3.0-or-later
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, ErrorKind, Write},
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const LINE_TIMEOUT: Duration = Duration::from_secs(15);

fn sandbox() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("airpodsd-cli-{}-{unique}", std::process::id()));
    fs::create_dir_all(root.join("config")).unwrap();
    fs::create_dir_all(root.join("runtime")).unwrap();
    root
}

fn airpodsd(root: &Path, command: &str) -> Command {
    let mut process = Command::new(env!("CARGO_BIN_EXE_airpodsd"));
    process
        .arg(command)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"));
    process
}

fn assert_offline(line: &str) {
    let snapshot: Value = serde_json::from_str(line).unwrap();
    assert_eq!(snapshot["schema"], 1);
    assert_eq!(snapshot["status"], "error");
    assert_eq!(snapshot["connected"], false);
    assert_eq!(snapshot["error"], "airpodsd daemon is unavailable");
    assert_eq!(snapshot["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn status_without_daemon_prints_error_snapshot_and_fails() {
    let root = sandbox();
    let output = airpodsd(&root, "status").output().unwrap();
    assert!(!output.status.success());
    assert_offline(std::str::from_utf8(&output.stdout).unwrap());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn watch_survives_daemon_outages_and_relays_snapshots() {
    let root = sandbox();
    let mut watch = airpodsd(&root, "watch")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = watch.stdout.take().unwrap();
    let (tx, lines) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let next = || {
        lines
            .recv_timeout(LINE_TIMEOUT)
            .expect("watch printed no line")
    };

    // No daemon: one offline snapshot, and the stream stays open.
    assert_offline(&next());
    thread::sleep(Duration::from_millis(1500));
    assert!(
        watch.try_wait().unwrap().is_none(),
        "watch exited while the daemon was down"
    );

    // A daemon appears: watch reconnects and relays its snapshots verbatim.
    let listener = UnixListener::bind(root.join("runtime/omarchy-airpods/control.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + LINE_TIMEOUT;
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("watch did not reconnect: {e}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    let mut request = String::new();
    BufReader::new(&stream).read_line(&mut request).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&request).unwrap()["command"],
        "watch"
    );
    let cell = json!({"percent": null, "charging": null});
    let snapshot = json!({
        "schema": 1, "configured": true, "status": "idle", "name": "Test Pods",
        "connected": false, "in_ear": [null, null],
        "battery": {"left": cell, "right": cell, "case": cell},
        "mode": null, "error": null, "paired_devices": [], "version": "0.0.0-test"
    });
    writeln!(stream, "{snapshot}").unwrap();
    assert_eq!(serde_json::from_str::<Value>(&next()).unwrap(), snapshot);

    // The daemon stops: a fresh offline snapshot, and the stream stays open.
    drop(stream);
    drop(listener);
    assert_offline(&next());
    assert!(
        watch.try_wait().unwrap().is_none(),
        "watch exited after the daemon stopped"
    );

    watch.kill().unwrap();
    watch.wait().unwrap();
    fs::remove_dir_all(root).unwrap();
}
