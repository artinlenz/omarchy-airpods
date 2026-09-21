// SPDX-License-Identifier: AGPL-3.0-only
use anyhow::{Context, Result};
use dbus::{arg::PropMap, nonblock::{Proxy, SyncConnection, stdintf::org_freedesktop_dbus::Properties}};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};
use crate::{bluetooth::Event, model::Snapshot};

const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const PATH: &str = "/org/mpris/MediaPlayer2";

#[derive(Clone, Copy, PartialEq)]
enum Wear { Both, One, Unavailable }

fn wear(snapshot: &Snapshot) -> Wear {
    if !snapshot.configured || !snapshot.connected { return Wear::Unavailable; }
    match snapshot.in_ear {
        [Some(true), Some(true)] => Wear::Both,
        [Some(true), Some(false)] | [Some(false), Some(true)] => Wear::One,
        _ => Wear::Unavailable,
    }
}

#[derive(PartialEq)]
struct Track { id: Option<String>, url: Option<String> }
struct PausedPlayer { owner: String, track: Track }

fn player(connection: &Arc<SyncConnection>, owner: String) -> Proxy<'static, Arc<SyncConnection>> {
    Proxy::new(owner, PATH, Duration::from_secs(2), connection.clone())
}

async fn track(proxy: &Proxy<'_, Arc<SyncConnection>>) -> Result<Option<Track>> {
    let metadata: PropMap = proxy.get(PLAYER, "Metadata").await?;
    let text = |key: &str| metadata.get(key).and_then(|v| v.0.as_str()).map(str::to_owned);
    let track = Track { id: text("mpris:trackid"), url: text("xesam:url") };
    Ok((track.id.is_some() || track.url.is_some()).then_some(track))
}

async fn pause_playing(connection: &Arc<SyncConnection>, snapshots: &watch::Receiver<Snapshot>, warnings: &mpsc::Sender<Event>) -> Result<Vec<PausedPlayer>> {
    let bus = Proxy::new("org.freedesktop.DBus", "/org/freedesktop/DBus", Duration::from_secs(2), connection.clone());
    let (names,): (Vec<String>,) = bus.method_call("org.freedesktop.DBus", "ListNames", ()).await?;
    let mut paused = Vec::new();
    for name in names.into_iter().filter(|name| name.starts_with("org.mpris.MediaPlayer2.")) {
        if wear(&snapshots.borrow()) != Wear::One { break; }
        let result: Result<Option<PausedPlayer>> = async {
            // Address the unique owner, not a reusable player name. A restarted
            // application must never inherit another instance's resume permit.
            let (owner,): (String,) = bus.method_call("org.freedesktop.DBus", "GetNameOwner", (name,)).await?;
            let proxy = player(connection, owner.clone());
            let status: String = proxy.get(PLAYER, "PlaybackStatus").await?;
            if status != "Playing" || !proxy.get::<bool>(PLAYER, "CanPause").await? { return Ok(None); }
            let identity = track(&proxy).await?;
            if wear(&snapshots.borrow()) != Wear::One { return Ok(None); }
            let (): () = proxy.method_call(PLAYER, "Pause", ()).await?;
            let status: String = proxy.get(PLAYER, "PlaybackStatus").await?;
            if status != "Paused" { anyhow::bail!("player did not confirm pause"); }
            let Some(track) = identity else {
                anyhow::bail!("player has no media identity; paused without automatic resume");
            };
            Ok(Some(PausedPlayer { owner, track }))
        }.await;
        match result {
            Ok(Some(record)) => paused.push(record),
            Ok(None) => {},
            Err(_) => warn(warnings, "A media player could not confirm pause/resume ownership").await,
        }
    }
    Ok(paused)
}

async fn still_owned(connection: &Arc<SyncConnection>, record: &PausedPlayer) -> Result<bool> {
    let proxy = player(connection, record.owner.clone());
    let status: String = proxy.get(PLAYER, "PlaybackStatus").await?;
    Ok(status == "Paused" && track(&proxy).await?.as_ref() == Some(&record.track))
}

async fn warn(warnings: &mpsc::Sender<Event>, message: &str) {
    let _ = warnings.send(Event::MediaWarning(message.into())).await;
}

pub async fn run(mut snapshots: watch::Receiver<Snapshot>, warnings: mpsc::Sender<Event>) {
    let result = dbus_tokio::connection::new_session_sync();
    let (resource, connection) = match result {
        Ok(value) => value,
        Err(_) => { warn(&warnings, "Session media controls are unavailable").await; return; }
    };
    let io = tokio::spawn(async move { let _ = resource.await; });
    let mut previous = Wear::Unavailable;
    let mut paused = Vec::new();
    while snapshots.changed().await.is_ok() {
        let current = wear(&snapshots.borrow_and_update());
        if current == Wear::Unavailable {
            // No delayed autoplay after link loss, shutdown, release or unknown
            // ear identity. A new session must establish its own transition.
            paused.clear();
        } else if previous == Wear::Both && current == Wear::One {
            match pause_playing(&connection, &snapshots, &warnings).await.context("media pause failed") {
                Ok(records) => paused = records,
                Err(_) => warn(&warnings, "Session media controls could not pause playback").await,
            }
        } else if current == Wear::Both {
            for record in paused.drain(..) {
                if wear(&snapshots.borrow()) != Wear::Both { break; }
                if !still_owned(&connection, &record).await.unwrap_or(false) { continue; }
                let proxy = player(&connection, record.owner);
                if wear(&snapshots.borrow()) != Wear::Both { break; }
                let result: Result<(), _> = proxy.method_call(PLAYER, "Play", ()).await;
                if result.is_err() { warn(&warnings, "A media player could not resume playback").await; }
            }
        } else if !paused.is_empty() {
            // Manual playback or a changed track revokes ownership while the
            // bud is out; never resume an unrelated or previously paused item.
            let mut retained = Vec::with_capacity(paused.len());
            for record in paused.drain(..) {
                if still_owned(&connection, &record).await.unwrap_or(false) { retained.push(record); }
            }
            paused = retained;
        }
        previous = current;
    }
    io.abort();
}
