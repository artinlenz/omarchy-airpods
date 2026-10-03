// SPDX-License-Identifier: MIT
import QtQuick
import Quickshell
import Quickshell.Io
import "Model.js" as Model

Item {
  id: root

  // The shell creates this service once, independently of monitor widgets.
  readonly property string executable: Quickshell.env("HOME") + "/.local/bin/airpodsd"
  readonly property int retryMinMs: 1000
  readonly property int retryMaxMs: 30000
  // A watch process must stay up this long before its restart backoff resets.
  readonly property int stableMs: 5000
  // Injected by the shell: this plugin's manifest.json, the single source of
  // the plugin version. `omarchy plugin update` never rebuilds airpodsd.
  property var manifest: null
  readonly property string pluginVersion: manifest && typeof manifest.version === "string" ? manifest.version : ""
  property var snapshot: Model.unavailable(false, "AirPods")
  property bool online: false
  readonly property bool outdated: online && pluginVersion !== "" && snapshot.version !== pluginVersion
  property bool stopping: false
  property int retryDelay: retryMinMs
  property string transportError: "airpodsd is unavailable. Waiting for its user service…"
  property string commandError: ""
  property string commandStderr: ""
  property bool busy: false
  property string action: ""
  readonly property string versionError: outdated
    ? "Backend outdated: run install.sh (airpodsd " + (snapshot.version || "unknown") + ", plugin " + pluginVersion + ")."
    : ""
  readonly property string error: [commandError, versionError, transportError, snapshot.error]
    .filter(function(message, index, messages) { return message && messages.indexOf(message) === index }).join("\n")
  readonly property bool canSetMode: online && !outdated && snapshot.connected
    && (snapshot.in_ear[0] === true || snapshot.in_ear[1] === true) && !busy

  function accept(line) {
    if (!line.trim()) return
    try {
      snapshot = Model.snapshot(JSON.parse(line))
      online = true
      transportError = ""
    } catch (error) {
      offline("Invalid status from airpodsd. Update the backend and plugin together.")
    }
  }

  function offline(message) {
    online = false
    snapshot = Model.unavailable(snapshot.configured, snapshot.name)
    transportError = message
  }

  function startWatch() {
    if (stopping || watch.running) return
    // Also covers failure to exec: Process does not emit exited in that case.
    reconnect.interval = retryDelay
    reconnect.restart()
    retryDelay = Math.min(retryDelay * 2, retryMaxMs)
    watch.running = true
  }

  function run(args, label) {
    if (busy || !online || outdated) return
    busy = true
    action = label
    commandError = ""
    commandStderr = ""
    command.command = [executable].concat(args)
    launchDeadline.restart()
    command.running = true
  }

  function setup(address) {
    if (snapshot.configured || !snapshot.paired_devices.some(function(device) { return device.address === address })) return
    run(["setup", address], "Setting up…")
  }

  function setMode(mode) {
    if (!canSetMode || ["off", "anc", "transparency", "adaptive"].indexOf(mode) < 0) return
    run(["mode", mode], "Changing listening mode…")
  }

  Component.onCompleted: startWatch()
  Component.onDestruction: {
    stopping = true
    reconnect.stop()
    stable.stop()
    launchDeadline.stop()
    watch.running = false
    command.running = false
  }

  Process {
    id: watch
    command: [root.executable, "watch"]
    stdout: SplitParser {
      onRead: function(line) { root.accept(line) }
    }
    stderr: SplitParser {
      onRead: function(line) {
        if (line.trim()) root.transportError = line.trim().slice(0, 1000)
      }
    }
    onStarted: {
      reconnect.stop()
      stable.restart()
    }
    onExited: function(exitCode, exitStatus) {
      stable.stop()
      if (root.stopping) return
      root.offline(root.transportError || "airpodsd is offline. Reconnecting…")
      reconnect.interval = root.retryDelay
      reconnect.restart()
    }
  }

  // Only a watch that stays up earns a fast restart. Resetting on the first
  // snapshot instead let a process that printed one line and exited respawn
  // every second.
  Timer {
    id: stable
    interval: root.stableMs
    onTriggered: root.retryDelay = root.retryMinMs
  }

  Timer {
    id: reconnect
    onTriggered: {
      if (watch.running || root.stopping) return
      root.offline(root.transportError || "Cannot start airpodsd. Install the backend and enable its user service.")
      root.startWatch()
    }
  }

  Process {
    id: command
    // Drain protocol output without letting a command race the watch snapshot.
    stdout: StdioCollector {}
    stderr: SplitParser {
      onRead: function(line) {
        if (line.trim()) root.commandStderr = (root.commandStderr + "\n" + line.trim()).trim().slice(-1000)
      }
    }
    onStarted: launchDeadline.stop()
    onExited: function(exitCode, exitStatus) {
      launchDeadline.stop()
      root.busy = false
      root.action = ""
      if (exitCode !== 0 || exitStatus !== 0)
        root.commandError = root.commandStderr || "airpodsd command failed (exit " + exitCode + ")."
    }
  }

  Timer {
    id: launchDeadline
    interval: 1500
    onTriggered: {
      if (command.running) return
      root.busy = false
      root.action = ""
      root.commandError = "Cannot start " + root.executable + ". Install the backend first."
    }
  }
}
