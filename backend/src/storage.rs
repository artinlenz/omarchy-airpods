// SPDX-License-Identifier: AGPL-3.0-only
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs::{self, File, OpenOptions}, io::{Read, Write}, os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt}, path::{Path, PathBuf}};

#[derive(Clone, Serialize, Deserialize)]
pub struct Keys { pub irk: [u8; 16], pub enc: [u8; 16] }
#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    pub schema: u8,
    pub adapter: String,
    pub address: String,
    pub name: String,
    pub original_blocked: bool,
    pub managed: bool,
    pub keys: Option<Keys>,
}
impl Config { pub fn configured(&self) -> bool { self.managed && self.keys.is_some() } }

pub fn config_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    if !base.is_absolute() { bail!("configuration directory must be absolute"); }
    Ok(base.join("omarchy-airpods/config.json"))
}

pub fn runtime_dir() -> Result<PathBuf> {
    let base = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?);
    if !base.is_absolute() { bail!("runtime directory must be absolute"); }
    let path = base.join("omarchy-airpods");
    private_dir(&path)?;
    Ok(path)
}

pub fn private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } { bail!("private directory has unsafe ownership or type"); }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
        }
        Err(e) => return Err(e.into()),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub fn private_open(path: &Path, create: bool) -> Result<File> {
    let file = OpenOptions::new().read(true).write(create).create(create).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } { bail!("private file has unsafe ownership or type"); }
    if meta.mode() & 0o077 != 0 { file.set_permissions(fs::Permissions::from_mode(0o600))?; }
    Ok(file)
}

pub fn load() -> Result<Option<Config>> {
    let path = config_path()?;
    if !path.try_exists()? { return Ok(None); }
    let mut bytes = Vec::new();
    private_open(&path, false)?.take(16385).read_to_end(&mut bytes)?;
    if bytes.len() > 16384 { bail!("configuration file is too large"); }
    // Do not include serde diagnostics or input snippets: this file contains keys.
    let config: Config = serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid private configuration"))?;
    if config.schema != 1 || config.address.parse::<bluer::Address>().is_err() || config.adapter.parse::<bluer::Address>().is_err() {
        bail!("invalid private configuration identity");
    }
    if config.keys.as_ref().is_some_and(|k| k.irk == [0; 16] || k.enc == [0; 16]) { bail!("invalid stored proximity keys; run setup again"); }
    Ok(Some(config))
}

pub fn save(config: &Config) -> Result<()> {
    let path = config_path()?;
    let dir = path.parent().context("configuration directory is missing")?;
    private_dir(dir)?;
    let temporary = dir.join(format!(".config-{}.tmp", std::process::id()));
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&temporary)?;
    let result = (|| -> Result<()> {
        serde_json::to_writer(&mut file, config)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(temporary); }
    result
}
