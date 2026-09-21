// SPDX-License-Identifier: AGPL-3.0-only
use anyhow::{bail, Context, Result};
use bluer::{Adapter, Session, l2cap::SeqPacket};
use std::{sync::Arc, time::Duration};
use tokio::{sync::{mpsc, oneshot, watch}, task::JoinHandle, time::{timeout, Instant}};
use crate::{bluetooth::{self, Event}, model::{wearing, Battery, Mode, Snapshot}, protocol::{self, Packet}, storage::{self, Config, Keys}};

pub enum Command { Setup(String), Mode(Mode), Release }
pub struct Request { pub command: Command, pub reply: oneshot::Sender<Result<Snapshot>> }
const FRESH: Duration = Duration::from_secs(3);

struct Actor {
    config: Option<Config>,
    session: Option<Session>,
    adapter: Option<Adapter>,
    scan: Option<JoinHandle<()>>,
    connection: Option<JoinHandle<()>>,
    socket: Option<Arc<SeqPacket>>,
    generation: u64,
    cutoff: Instant,
    last_ble: Option<Instant>,
    ear_deadline: Option<Instant>,
    live_ears: bool,
    require_removal: bool,
    primary_left: Option<bool>,
    aap_ears: [Option<bool>; 2],
    pending_mode: Option<(Mode, Instant)>,
    audio: Option<JoinHandle<()>>,
    snapshot: Snapshot,
    snapshots: watch::Sender<Snapshot>,
    tx: mpsc::Sender<Event>,
}

impl Actor {
    fn publish(&self) { self.snapshots.send_replace(self.snapshot.clone()); }
    fn reset_state(&mut self) {
        self.snapshot.connected = false;
        self.snapshot.in_ear = [None; 2];
        self.snapshot.battery = Battery::default();
        self.snapshot.mode = None;
        self.pending_mode = None;
        self.snapshot.configured = self.config.as_ref().is_some_and(Config::configured);
        self.snapshot.status = if self.snapshot.configured { "idle" } else { "setup_required" }.into();
        self.live_ears = false;
        self.primary_left = None;
        self.aap_ears = [None; 2];
        self.last_ble = None;
        self.ear_deadline = None;
        self.cutoff = Instant::now();
        self.generation = self.generation.wrapping_add(1);
    }
    async fn guard(&mut self) -> Result<()> {
        if let Some(task) = self.connection.take() { task.abort(); }
        if let Some(task) = self.audio.take() { task.abort(); }
        self.socket = None;
        self.reset_state();
        if let (Some(adapter), Some(config)) = (&self.adapter, &self.config) {
            if config.managed { bluetooth::block(adapter, config).await?; }
        }
        Ok(())
    }
    fn error(&mut self, error: impl std::fmt::Display) {
        self.snapshot.status = "error".into();
        self.snapshot.error = Some(error.to_string());
    }
    async fn initialize(&mut self) -> Result<()> {
        let session = Session::new().await.context("BlueZ system service is unavailable")?;
        let adapter = bluetooth::adapter(&session, self.config.as_ref()).await?;
        self.adapter = Some(adapter.clone());
        self.session = Some(session);
        // Install protection even while the radio is off, before discovery.
        self.guard().await?;
        self.snapshot.paired_devices = bluetooth::paired(&adapter).await?;
        if !adapter.is_powered().await? { bail!("Bluetooth adapter is powered off"); }
        if let Some(config) = self.config.as_ref().filter(|c| c.configured()) {
            self.scan = Some(bluetooth::scanner(adapter, config.clone(), self.tx.clone()).await?);
        }
        self.snapshot.error = self.require_removal.then(|| "Connection interrupted; remove both buds before retrying".into());
        Ok(())
    }
    async fn offline(&mut self, reason: String) {
        if let Some(task) = self.scan.take() { task.abort(); }
        let guarded = self.guard().await;
        self.adapter = None;
        self.session = None;
        self.error(match guarded { Ok(()) => reason, Err(e) => format!("{reason}; guard could not be confirmed: {e:#}") });
    }
    async fn setup(&mut self, address: String) -> Result<()> {
        let address: bluer::Address = address.parse().context("invalid Bluetooth address")?;
        if self.config.as_ref().is_some_and(|c| c.managed && c.address != address.to_string()) { bail!("release the managed device before selecting another"); }
        if self.adapter.is_none() { self.initialize().await?; }
        let adapter = self.adapter.clone().context("Bluetooth adapter is unavailable")?;
        if !adapter.is_powered().await? { bail!("Bluetooth adapter is powered off"); }
        let device = adapter.device(address)?;
        if !device.is_paired().await? { bail!("setup requires an already paired device"); }
        let existing = self.config.as_ref().filter(|c| c.managed && c.address == address.to_string());
        let original_blocked = match existing { Some(c) => c.original_blocked, None => device.is_blocked().await? };
        if existing.is_none() && original_blocked { bail!("selected device was already blocked; unblock it explicitly before setup"); }
        let mut config = Config {
            schema: 1, adapter: adapter.address().await?.to_string(), address: address.to_string(),
            name: device.name().await?.unwrap_or_else(|| "AirPods".into()), original_blocked, managed: true,
            keys: existing.and_then(|c| c.keys.clone()),
        };
        // Persist ownership BEFORE any temporary unblock. A crash during setup
        // is recoverable with guard/release, even before keys have arrived.
        storage::save(&config)?;
        self.config = Some(config.clone());
        if let Some(task) = self.scan.take() { task.abort(); }
        self.guard().await?;
        self.snapshot.status = "connecting".into();
        self.snapshot.name = config.name.clone();
        self.snapshot.error = None;
        self.publish();
        let result = timeout(Duration::from_secs(25), async {
            device.set_blocked(false).await?;
            device.connect().await.context("setup Bluetooth connection failed")?;
            let channel = bluetooth::attach(address).await?;
            bluetooth::send(&channel, protocol::KEYS_REQUEST).await?;
            let mut received = protocol::ReceivedKeys::default();
            let mut buffer = [0; 1024];
            loop {
                let len = channel.recv(&mut buffer).await?;
                if len == 0 { bail!("AAP connection closed before proximity keys arrived"); }
                if let Some(Packet::Keys(keys)) = protocol::parse(&buffer[..len]) {
                    if let Some(key) = keys.irk { received.irk = Some(key); }
                    if let Some(key) = keys.enc { received.enc = Some(key); }
                    if let (Some(irk), Some(enc)) = (received.irk, received.enc) { return Ok::<_, anyhow::Error>(Keys { irk, enc }); }
                }
            }
        }).await.context("setup timed out waiting for valid proximity keys").and_then(|r| r);
        // Always end the authorized setup exception, including malformed keys,
        // timeout, pairing errors, or a failed atomic write.
        let guard_result = self.guard().await;
        match result {
            Ok(keys) => {
                config.keys = Some(keys);
                storage::save(&config)?;
                self.config = Some(config);
            }
            Err(e) => { guard_result?; return Err(e); }
        }
        guard_result?;
        self.reset_state();
        self.require_removal = false;
        self.scan = Some(bluetooth::scanner(adapter, self.config.clone().unwrap(), self.tx.clone()).await?);
        Ok(())
    }
    async fn release(&mut self) -> Result<()> {
        if let Some(task) = self.scan.take() { task.abort(); }
        self.guard().await?;
        let Some(mut config) = self.config.clone().filter(|c| c.managed) else { return Ok(()); };
        let adapter = self.adapter.as_ref().context("Bluetooth is unavailable; cannot restore original device setting")?;
        let device = adapter.device(config.address.parse()?)?;
        // Restore first, then relinquish ownership. A crash between these two
        // steps remains safely recoverable: guard can re-block, release retries.
        device.set_blocked(config.original_blocked).await?;
        config.managed = false;
        if let Err(e) = storage::save(&config) {
            device.set_blocked(true).await.context("release save failed and guard could not be restored")?;
            return Err(e);
        }
        self.config = Some(config);
        self.require_removal = false;
        self.reset_state();
        self.snapshot.error = None;
        Ok(())
    }
    async fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Setup(address) => self.setup(address).await,
            Command::Release => self.release().await,
            Command::Mode(mode) => {
                if !self.snapshot.connected || !self.live_ears || !wearing(self.snapshot.in_ear) { bail!("noise control requires a live in-ear connection"); }
                let channel = self.socket.as_ref().context("AAP control channel is unavailable")?;
                bluetooth::send(channel, &protocol::mode_packet(mode)).await?;
                self.pending_mode = (self.snapshot.mode != Some(mode)).then(|| (mode, Instant::now() + Duration::from_secs(5)));
                self.snapshot.error = None;
                // Do not optimistically claim the mode; the device notification
                // is the source of truth, including unsupported mode requests.
                Ok(())
            }
        }
    }
    async fn event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Advertisement { data, at, fresh } => {
                if at <= self.cutoff || at.elapsed() > FRESH { return Ok(()); }
                let Some(keys) = self.config.as_ref().filter(|c| c.configured()).and_then(|c| c.keys.as_ref()) else { return Ok(()); };
                let Some(ad) = protocol::advertisement(&data, &keys.enc) else {
                    if fresh && !self.live_ears && (self.connection.is_some() || self.socket.is_some()) {
                        self.guard().await?;
                    }
                    return Ok(());
                };
                // A live notification-based AAP state is not expired by absent
                // or stale BLE reports, nor overwritten by older broadcasts.
                if self.live_ears { return Ok(()); }
                self.snapshot.in_ear = ad.ears;
                self.snapshot.battery = ad.battery;
                if !fresh { return Ok(()); }
                self.last_ble = Some(at);
                if !wearing(ad.ears) {
                    if ad.ears == [Some(false); 2] { self.require_removal = false; }
                    let ears = ad.ears;
                    if self.connection.is_some() || self.socket.is_some() { self.guard().await?; }
                    self.snapshot.in_ear = ears;
                } else if !self.require_removal && self.connection.is_none() && self.socket.is_none() {
                    let adapter = self.adapter.clone().context("Bluetooth adapter is unavailable")?;
                    let config = self.config.clone().context("device is not configured")?;
                    // No fallback connects on lid, model, name, RSSI, or merely
                    // advertised disconnected state. Each attempt consumes a
                    // newly received, identity-resolved positive observation.
                    if at.elapsed() > FRESH { return Ok(()); }
                    // One attempt per wearing transition. A failed AAP check
                    // must not cause an endless connect/disconnect loop.
                    self.require_removal = true;
                    adapter.device(config.address.parse()?)?.set_blocked(false).await?;
                    self.snapshot.status = "connecting".into();
                    self.snapshot.error = None;
                    self.connection = Some(bluetooth::connect(adapter, config, self.generation, self.tx.clone()));
                }
            }
            Event::Ready { generation, result } => {
                if generation != self.generation { return Ok(()); }
                self.connection = None;
                let socket = result?;
                if !wearing(self.snapshot.in_ear) {
                    drop(socket);
                    self.guard().await?;
                    return Ok(());
                }
                self.socket = Some(socket);
                self.ear_deadline = Some(Instant::now() + Duration::from_secs(5));
                // Wait for actual AAP ear confirmation before exposing controls.
            }
            Event::Connected(false) if self.socket.is_some() || self.connection.is_some() => { self.guard().await?; }
            Event::Connected(true) | Event::Blocked(false) if self.socket.is_none() && self.connection.is_none() => { self.guard().await?; }
            Event::Failure(message) => { self.offline(message).await; }
            Event::AudioWarning { generation, message } if generation == self.generation && self.live_ears => {
                self.snapshot.error = Some(message);
            }
            _ => {}
        }
        Ok(())
    }
    fn sided_ears(&self) -> [Option<bool>; 2] {
        match self.primary_left {
            Some(true) => self.aap_ears,
            Some(false) => [self.aap_ears[1], self.aap_ears[0]],
            None => [None; 2],
        }
    }
    async fn packet(&mut self, packet: &[u8]) -> Result<()> {
        match protocol::parse(packet) {
            Some(Packet::Ear(ears)) => {
                self.aap_ears = ears;
                let sided = self.sided_ears();
                if !wearing(ears) {
                    if ears == [Some(false); 2] { self.require_removal = false; }
                    self.guard().await?;
                    self.snapshot.in_ear = sided;
                } else {
                    let first = !self.live_ears;
                    self.live_ears = true;
                    self.ear_deadline = None;
                    self.snapshot.in_ear = sided;
                    self.snapshot.connected = true;
                    self.snapshot.status = "connected".into();
                    if first {
                        self.publish();
                        if let Some(config) = &self.config {
                            let address = config.address.clone();
                            let tx = self.tx.clone();
                            let generation = self.generation;
                            self.audio = Some(tokio::spawn(async move {
                                if let Err(e) = bluetooth::prefer_aac(&address).await {
                                    let _ = tx.send(Event::AudioWarning { generation, message: format!("Connected; AAC preference unavailable: {e:#}") }).await;
                                }
                            }));
                        }
                    }
                }
            }
            Some(Packet::Batteries { battery, primary_left }) => {
                self.snapshot.battery = battery;
                self.primary_left = primary_left;
                if self.live_ears { self.snapshot.in_ear = self.sided_ears(); }
            }
            Some(Packet::Mode(mode)) => {
                self.snapshot.mode = mode;
                if self.pending_mode.is_some_and(|(requested, _)| mode == Some(requested)) {
                    self.pending_mode = None;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

pub async fn run(mut requests: mpsc::Receiver<Request>, snapshots: watch::Sender<Snapshot>, mut shutdown: oneshot::Receiver<()>) -> Result<()> {
    let config = storage::load()?;
    let (tx, mut events) = mpsc::channel(64);
    let mut actor = Actor {
        config, session: None, adapter: None, scan: None, connection: None, socket: None,
        generation: 0, cutoff: Instant::now(), last_ble: None, ear_deadline: None, live_ears: false, require_removal: false,
        primary_left: None, aap_ears: [None; 2], pending_mode: None, audio: None,
        snapshot: Snapshot::default(), snapshots, tx,
    };
    if let Some(config) = &actor.config { actor.snapshot.name = config.name.clone(); }
    actor.reset_state();
    if let Err(e) = actor.initialize().await { actor.offline(format!("{e:#}")).await; }
    actor.publish();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut maintenance = Instant::now();
    let mut buffer = [0; 1024];
    loop {
        let socket = actor.socket.clone();
        tokio::select! {
            _ = &mut shutdown => break,
            request = requests.recv() => {
                let Some(request) = request else { break; };
                let changes_ownership = !matches!(&request.command, Command::Mode(_));
                let result = actor.command(request.command).await;
                if let Err(e) = &result {
                    if changes_ownership { actor.offline(format!("{e:#}")).await; }
                    else { actor.snapshot.error = Some(format!("{e:#}")); }
                }
                actor.publish();
                let _ = request.reply.send(result.map(|_| actor.snapshot.clone()));
            }
            Some(event) = events.recv() => {
                if let Err(e) = actor.event(event).await {
                    actor.offline(format!("{e:#}")).await;
                }
                actor.publish();
            }
            received = async {
                match &socket { Some(s) => s.recv(&mut buffer).await, None => std::future::pending().await }
            } => {
                match received {
                    Ok(0) | Err(_) => {
                        let result = actor.guard().await;
                        if let Err(e) = result { actor.offline(format!("AAP link lost; guard failed: {e:#}")).await; }
                    }
                    Ok(len) => if let Err(e) = actor.packet(&buffer[..len]).await {
                        actor.offline(format!("{e:#}")).await;
                    }
                }
                actor.publish();
            }
            _ = tick.tick() => {
                if actor.pending_mode.is_some_and(|(_, deadline)| Instant::now() >= deadline) {
                    actor.pending_mode = None;
                    actor.snapshot.error = Some("Device did not confirm the requested listening mode; showing its reported mode".into());
                }
                if actor.ear_deadline.is_some_and(|t| Instant::now() >= t) {
                    match actor.guard().await {
                        Ok(()) => actor.error("AAP did not confirm in-ear status within five seconds; remove both buds before retrying"),
                        Err(e) => actor.offline(format!("AAP confirmation timed out; guard failed: {e:#}")).await,
                    }
                } else if actor.socket.is_none() && actor.connection.is_none() && actor.last_ble.is_some_and(|t| t.elapsed() > FRESH) {
                    actor.snapshot.in_ear = [None; 2];
                    actor.snapshot.battery = Battery::default();
                    actor.last_ble = None;
                }
                if maintenance.elapsed() >= Duration::from_secs(5) {
                    maintenance = Instant::now();
                    if actor.adapter.is_none() {
                        if let Err(e) = actor.initialize().await { actor.offline(format!("{e:#}")).await; }
                    } else if let Some(adapter) = actor.adapter.clone() {
                        match adapter.is_powered().await {
                            Ok(true) => match bluetooth::paired(&adapter).await {
                                Ok(paired) => actor.snapshot.paired_devices = paired,
                                Err(e) => actor.offline(format!("{e:#}")).await,
                            },
                            Ok(false) => actor.offline("Bluetooth adapter is powered off".into()).await,
                            Err(e) => actor.offline(format!("{e:#}")).await,
                        }
                    }
                }
                actor.publish();
            }
        }
    }
    if let Some(task) = actor.scan.take() { task.abort(); }
    actor.guard().await
}

pub async fn offline_guard(release: bool) -> Result<()> {
    let Some(mut config) = storage::load()?.filter(|c| c.managed) else { return Ok(()); };
    let session = Session::new().await.context("BlueZ is unavailable; device guard cannot be changed")?;
    let adapter = bluetooth::adapter(&session, Some(&config)).await?;
    if release {
        let device = adapter.device(config.address.parse()?)?;
        device.set_blocked(config.original_blocked).await?;
        config.managed = false;
        if let Err(e) = storage::save(&config) {
            device.set_blocked(true).await.context("release save failed and guard could not be restored")?;
            return Err(e);
        }
    } else { bluetooth::block(&adapter, &config).await?; }
    Ok(())
}
