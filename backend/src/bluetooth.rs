// SPDX-License-Identifier: AGPL-3.0-only
use anyhow::{anyhow, bail, Context, Result};
use bluer::{Adapter, AdapterEvent, AdapterProperty, Address, AddressType, DeviceEvent, DeviceProperty, DiscoveryFilter, DiscoveryTransport, Session, l2cap::{SeqPacket, Socket, SocketAddr}};
use futures::{StreamExt, stream::SelectAll};
use std::{collections::{HashMap, HashSet}, sync::Arc, time::Duration};
use tokio::{sync::mpsc, task::JoinHandle, time::{timeout, Instant}};
use crate::{model::PairedDevice, protocol, storage::Config};

pub enum Event {
    Advertisement { data: Vec<u8>, at: Instant, fresh: bool },
    Connected(bool),
    Blocked(bool),
    Ready { generation: u64, result: Result<Arc<SeqPacket>> },
    Failure(String),
    AudioWarning { generation: u64, message: String },
}

pub async fn adapter(session: &Session, configured: Option<&Config>) -> Result<Adapter> {
    if let Some(config) = configured {
        let wanted: Address = config.adapter.parse()?;
        for name in session.adapter_names().await? {
            let a = session.adapter(&name)?;
            if a.address().await? == wanted { return Ok(a); }
        }
        bail!("configured Bluetooth adapter is unavailable");
    }
    Ok(session.default_adapter().await?)
}

pub async fn paired(adapter: &Adapter) -> Result<Vec<PairedDevice>> {
    let mut devices = Vec::new();
    for address in adapter.device_addresses().await? {
        let d = adapter.device(address)?;
        if d.is_paired().await? {
            devices.push(PairedDevice { address: address.to_string(), name: d.name().await?.unwrap_or_else(|| address.to_string()) });
        }
    }
    devices.sort_by(|a, b| a.name.cmp(&b.name).then(a.address.cmp(&b.address)));
    Ok(devices)
}

pub async fn block(adapter: &Adapter, config: &Config) -> Result<()> {
    let device = adapter.device(config.address.parse()?)?;
    // Block first closes the incoming-connection race; Disconnect also cancels
    // a Connect in progress. BlueZ itself disconnects all profiles on Blocked.
    device.set_blocked(true).await.context("cannot install device connection guard")?;
    let disconnected = device.disconnect().await;
    if !device.is_blocked().await? || device.is_connected().await? {
        disconnected.context("cannot disconnect guarded device")?;
        bail!("device connection guard could not be confirmed");
    }
    Ok(())
}

pub async fn send(socket: &SeqPacket, packet: &[u8]) -> Result<()> {
    let n = timeout(Duration::from_secs(3), socket.send(packet)).await.context("AAP write timed out")??;
    if n != packet.len() { bail!("short AAP packet write"); }
    Ok(())
}

pub async fn attach(address: Address) -> Result<Arc<SeqPacket>> {
    let socket = Socket::new_seq_packet()?;
    let channel = timeout(Duration::from_secs(10), socket.connect(SocketAddr::new(address, AddressType::BrEdr, 0x1001)))
        .await.context("AAP connect timed out")?.context("AAP control channel unavailable")?;
    let channel = Arc::new(channel);
    send(&channel, protocol::HANDSHAKE).await?;
    send(&channel, protocol::NOTIFICATIONS).await?;
    Ok(channel)
}

pub fn connect(adapter: Adapter, config: Config, generation: u64, tx: mpsc::Sender<Event>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = timeout(Duration::from_secs(15), async {
            let address = config.address.parse()?;
            adapter.device(address)?.connect().await.context("Bluetooth connection failed")?;
            attach(address).await
        }).await.unwrap_or_else(|_| Err(anyhow!("Bluetooth connection timed out")));
        let _ = tx.send(Event::Ready { generation, result }).await;
    })
}

fn recognized(address: Address, config: &Config) -> bool {
    let Some(keys) = &config.keys else { return false; };
    protocol::resolve_rpa(address.0, &keys.irk)
}

async fn emit_data(data: HashMap<u16, Vec<u8>>, tx: &mpsc::Sender<Event>, fresh: bool) -> Result<()> {
    if let Some(data) = data.get(&76) {
        tx.send(Event::Advertisement { data: data.clone(), at: Instant::now(), fresh }).await?;
    }
    Ok(())
}

pub async fn scanner(adapter: Adapter, config: Config, tx: mpsc::Sender<Event>) -> Result<JoinHandle<()>> {
    // Never power on or make the adapter discoverable. This is a per-client,
    // connection-free LE discovery session; dropping it releases only ours.
    adapter.set_discovery_filter(DiscoveryFilter {
        transport: DiscoveryTransport::Le,
        duplicate_data: true,
        discoverable: false,
        ..Default::default()
    }).await?;
    let known: HashSet<_> = adapter.device_addresses().await?.into_iter().collect();
    let discovery = adapter.discover_devices().await?;
    let target: Address = config.address.parse()?;
    Ok(tokio::spawn(async move {
        let result = async {
            futures::pin_mut!(discovery);
            let mut watching = HashSet::new();
            let mut changes = SelectAll::new();
            // Subscribe to the managed device before consuming discovery so
            // unsolicited ACL connections and external unblock are guarded.
            let d = adapter.device(target)?;
            let events = d.events().await?;
            changes.push(events.map(move |e| (target, e)).boxed());
            watching.insert(target);
            loop {
                tokio::select! {
                    event = discovery.next() => match event {
                        Some(AdapterEvent::DeviceAdded(address)) => {
                            if !recognized(address, &config) || !watching.insert(address) { continue; }
                            let d = adapter.device(address)?;
                            if d.address_type().await? != AddressType::LeRandom { continue; }
                            let events = d.events().await?;
                            changes.push(events.map(move |e| (address, e)).boxed());
                            // Initial cached objects are rendered but never authorize.
                            // Newly discovered objects were received during this scan.
                            if let Some(data) = d.manufacturer_data().await? { emit_data(data, &tx, !known.contains(&address)).await?; }
                        }
                        Some(AdapterEvent::DeviceRemoved(address)) => {
                            watching.remove(&address);
                            if address == target { bail!("configured Bluetooth device was removed"); }
                        }
                        Some(AdapterEvent::PropertyChanged(AdapterProperty::Powered(false))) => bail!("Bluetooth adapter is powered off"),
                        None => bail!("Bluetooth LE discovery stopped"),
                        _ => {}
                    },
                    Some((address, event)) = changes.next(), if !changes.is_empty() => {
                        match event {
                            DeviceEvent::PropertyChanged(DeviceProperty::ManufacturerData(data)) if recognized(address, &config) => emit_data(data, &tx, true).await?,
                            DeviceEvent::PropertyChanged(DeviceProperty::Connected(connected)) if address == target => { tx.send(Event::Connected(connected)).await?; }
                            DeviceEvent::PropertyChanged(DeviceProperty::Blocked(blocked)) if address == target => { tx.send(Event::Blocked(blocked)).await?; }
                            _ => {}
                        }
                    }
                    _ = tx.closed() => return Ok::<_, anyhow::Error>(()),
                }
            }
        }.await;
        if let Err(e) = result { let _ = tx.send(Event::Failure(format!("{e:#}"))).await; }
    }))
}

// Audio integration is device-specific. Selecting a reported available AAC
// profile is a preference, not a reason to fail or disconnect working audio.
fn aac_profile(card: &serde_json::Value) -> Option<&str> {
    card["profiles"].as_object()?.iter().find_map(|(name, profile)| {
        let description = profile["description"].as_str().unwrap_or_default();
        (name.contains("a2dp")
            && (name.to_ascii_lowercase().contains("aac") || description.to_ascii_lowercase().contains("aac"))
            && profile["available"].as_str() != Some("no")).then_some(name.as_str())
    })
}

pub async fn prefer_aac(address: &str) -> Result<()> {
    let mut command = tokio::process::Command::new("pactl");
    command.args(["--format=json", "list", "cards"]).kill_on_drop(true);
    let output = timeout(Duration::from_secs(3), command.output()).await.context("audio profile query timed out")??;
    if !output.status.success() { bail!("audio profile query failed"); }
    let cards: serde_json::Value = serde_json::from_slice(&output.stdout).context("invalid audio card response")?;
    let Some(cards) = cards.as_array() else { bail!("invalid audio card list"); };
    for card in cards {
        let properties = &card["properties"];
        let matches = ["api.bluez5.address", "device.string"].iter().any(|k| properties[*k].as_str().is_some_and(|s| s.eq_ignore_ascii_case(address)));
        if !matches { continue; }
        if let Some(profile) = aac_profile(card) {
            if card["active_profile"].as_str() == Some(profile) { return Ok(()); }
            let name = card["name"].as_str().context("audio card name is missing")?;
            let mut set = tokio::process::Command::new("pactl");
            set.args(["set-card-profile", name, profile]).kill_on_drop(true);
            let out = timeout(Duration::from_secs(3), set.output()).await.context("AAC profile selection timed out")??;
            if !out.status.success() { bail!("AAC profile selection failed"); }
        }
        return Ok(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aac_selection_uses_description_and_rejects_unavailable_profiles() {
        let mut card = serde_json::json!({"profiles": {
            "a2dp-sink": {"description": "High Fidelity Playback (A2DP Sink, codec AAC)", "available": "yes"},
            "a2dp-sink-sbc": {"description": "High Fidelity Playback (SBC)", "available": "yes"}
        }});
        assert_eq!(aac_profile(&card), Some("a2dp-sink"));
        card["profiles"]["a2dp-sink"]["available"] = "no".into();
        assert_eq!(aac_profile(&card), None);
    }
}
