// SPDX-License-Identifier: MIT
.pragma library

function nullableBool(value) {
  return typeof value === "boolean" ? value : null
}

function battery(value) {
  var percent = value ? value.percent : null
  return Object.freeze({
    percent: typeof percent === "number" && isFinite(percent) && percent >= 0 && percent <= 100 ? percent : null,
    charging: nullableBool(value ? value.charging : null)
  })
}

function unavailable(configured, name) {
  return snapshot({
    schema: 1, configured: configured === true, name: name || "AirPods",
    status: "error", connected: false, in_ear: [null, null],
    battery: {}, mode: null, error: null, paired_devices: []
  })
}

function snapshot(value) {
  if (!value || value.schema !== 1 || typeof value.configured !== "boolean"
      || typeof value.connected !== "boolean" || !Array.isArray(value.in_ear)
      || !value.battery || !Array.isArray(value.paired_devices)
      || ["setup_required", "idle", "connecting", "connected", "error"].indexOf(value.status) < 0)
    throw new Error("Unsupported airpodsd snapshot")

  var devices = value.paired_devices.filter(function(device) {
    return device && typeof device.address === "string" && typeof device.name === "string"
  }).map(function(device) {
    return Object.freeze({address: device.address, name: device.name})
  })
  return Object.freeze({
    schema: 1,
    configured: value.configured,
    status: value.status,
    name: typeof value.name === "string" && value.name.length ? value.name : "AirPods",
    connected: value.connected,
    in_ear: Object.freeze([nullableBool(value.in_ear[0]), nullableBool(value.in_ear[1])]),
    battery: Object.freeze({
      left: battery(value.battery.left),
      right: battery(value.battery.right),
      case: battery(value.battery.case)
    }),
    mode: ["off", "anc", "transparency", "adaptive"].indexOf(value.mode) >= 0 ? value.mode : null,
    error: typeof value.error === "string" ? value.error : null,
    paired_devices: Object.freeze(devices)
  })
}

function percent(value) {
  return value === null || value === undefined ? "—" : Math.round(value) + "%"
}

function barPercent(state) {
  var left = state.battery.left.percent
  var right = state.battery.right.percent
  if (left !== null && right !== null) return percent(Math.min(left, right))
  if (left !== null) return percent(left)
  if (right !== null) return percent(right)
  return ""
}

function ear(value) {
  return value === true ? "In ear" : value === false ? "Not in ear" : "Ear status unknown"
}
