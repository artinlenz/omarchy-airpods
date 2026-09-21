// SPDX-License-Identifier: AGPL-3.0-only
use std::{fs, process::Command, time::{SystemTime, UNIX_EPOCH}};

#[test]
fn absent_daemon_flushes_error_snapshot_before_exiting() {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let root = std::env::temp_dir().join(format!("airpodsd-cli-{}-{unique}", std::process::id()));
    fs::create_dir_all(root.join("config")).unwrap();
    fs::create_dir_all(root.join("runtime")).unwrap();
    for command in ["status", "watch"] {
        let output = Command::new(env!("CARGO_BIN_EXE_airpodsd"))
            .arg(command)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .output().unwrap();
        assert!(!output.status.success());
        let snapshot: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(snapshot["schema"], 1);
        assert_eq!(snapshot["status"], "error");
        assert_eq!(snapshot["connected"], false);
        assert!(snapshot["error"].is_string());
    }
    fs::remove_dir_all(root).unwrap();
}
