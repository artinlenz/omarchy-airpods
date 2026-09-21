// SPDX-License-Identifier: MIT
import QtQuick
import QtQuick.Controls as Controls
import qs.Ui as Ui
import qs.Commons
import "Model.js" as Model

Ui.Panel {
  id: root
  moduleName: "artinlenz.airpods"
  ipcTarget: moduleName
  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  readonly property var service: bar && bar.shell ? bar.shell.serviceFor(moduleName) : null
  readonly property var snapshot: service ? service.snapshot : Model.unavailable(false, "AirPods")
  readonly property bool online: !!service && service.online
  readonly property bool busy: !!service && service.busy
  readonly property string errorText: service ? service.error : "AirPods service is unavailable. Enable this plugin in the built-in Omarchy bar."
  readonly property string percentText: Model.barPercent(snapshot)
  readonly property string statusText: {
    if (!online) return "Backend unavailable"
    if (busy) return service.action
    if (!snapshot.configured) return "One-time setup required"
    if (snapshot.status === "connecting") return "Connecting…"
    if (snapshot.connected) return "Connected"
    if (snapshot.status === "error") return "Connection unavailable"
    return "Disconnected · waiting for in-ear detection"
  }
  readonly property var deviceOptions: [{value: "", label: "Select paired AirPods"}].concat(
    snapshot.paired_devices.map(function(device) {
      return {value: device.address, label: device.name + " · " + device.address}
    }))
  property string selectedAddress: ""

  onDeviceOptionsChanged: {
    if (!snapshot.paired_devices.some(function(device) { return device.address === root.selectedAddress }))
      selectedAddress = ""
  }
  onOpenedChanged: if (!opened) devices.close()

  Ui.WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: "\uf025" + (root.percentText ? (vertical ? "\n" : " ") + root.percentText : "")
    fontSize: Style.font.body
    horizontalMargin: 6
    dimmed: root.online && !root.snapshot.connected
    active: root.errorText !== ""
    tooltipText: root.snapshot.name + " · " + root.statusText
      + (root.percentText ? " · " + root.percentText + " (lower available earbud)" : "")
    onPressed: function(mouseButton) { if (mouseButton === Qt.LeftButton) root.toggle() }
  }

  Ui.KeyboardPanel {
    id: popup
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: keys
    contentWidth: popup.fittedContentWidth(Math.min(360, Style.space(340)))
    contentHeight: popup.fittedContentHeight(content.implicitHeight)

    FocusScope {
      id: keys
      anchors.fill: parent
      Keys.onEscapePressed: function(event) { root.close(); event.accepted = true }

      Flickable {
        id: scroll
        anchors.fill: parent
        contentWidth: width
        contentHeight: content.implicitHeight
        clip: true
        boundsBehavior: Flickable.StopAtBounds
        Controls.ScrollBar.vertical: Controls.ScrollBar {}

        Column {
          id: content
          width: scroll.width
          spacing: Style.space(12)

          Item {
            width: parent.width
            implicitHeight: Math.max(heading.implicitHeight, closeButton.implicitHeight)
            Column {
              id: heading
              anchors.left: parent.left
              anchors.right: closeButton.left
              anchors.rightMargin: Style.space(8)
              spacing: Style.space(3)
              Text {
                width: parent.width
                textFormat: Text.PlainText
                text: root.snapshot.name
                color: Color.popups.text
                font.family: Style.font.family
                font.pixelSize: Style.font.title
                font.bold: true
                elide: Text.ElideRight
              }
              Text {
                width: parent.width
                textFormat: Text.PlainText
                text: root.statusText
                color: Color.popups.text
                opacity: 0.7
                font.family: Style.font.family
                font.pixelSize: Style.font.caption
                wrapMode: Text.Wrap
              }
            }
            Ui.Button {
              id: closeButton
              anchors.right: parent.right
              text: "×"
              tooltipText: "Close (Escape)"
              foreground: Color.popups.text
              focusable: true
              onClicked: root.close()
            }
          }

          Row {
            width: parent.width
            spacing: Style.space(6)
            Repeater {
              model: ["left", "right", "case"]
              delegate: Rectangle {
                id: batteryCard
                required property string modelData
                required property int index
                readonly property var reading: root.snapshot.battery[modelData]
                width: (parent.width - Style.space(12)) / 3
                height: batteryLabels.implicitHeight + Style.space(16)
                radius: Style.cornerRadius
                color: Style.selectedFillFor(Color.popups.text, Color.accent)
                Column {
                  id: batteryLabels
                  anchors.left: parent.left
                  anchors.right: parent.right
                  anchors.top: parent.top
                  anchors.margins: Style.space(8)
                  spacing: Style.space(4)
                  Text {
                    width: parent.width
                    textFormat: Text.PlainText
                    text: ["Left", "Right", "Case"][batteryCard.index]
                    color: Color.popups.text
                    font.family: Style.font.family
                    font.pixelSize: Style.font.caption
                    horizontalAlignment: Text.AlignHCenter
                  }
                  Text {
                    width: parent.width
                    textFormat: Text.PlainText
                    text: (batteryCard.reading.charging === true ? "\uf0e7 " : "") + Model.percent(batteryCard.reading.percent)
                    color: Color.popups.text
                    font.family: Style.font.family
                    font.pixelSize: Style.font.title
                    horizontalAlignment: Text.AlignHCenter
                  }
                  Text {
                    width: parent.width
                    textFormat: Text.PlainText
                    text: batteryCard.reading.charging === true ? "Charging"
                      : batteryCard.reading.charging === false ? "Not charging" : "Charge unknown"
                    color: Color.popups.text
                    opacity: 0.7
                    font.family: Style.font.family
                    font.pixelSize: Style.font.caption
                    horizontalAlignment: Text.AlignHCenter
                    elide: Text.ElideRight
                  }
                }
              }
            }
          }

          Text {
            width: parent.width
            textFormat: Text.PlainText
            text: "L: " + Model.ear(root.snapshot.in_ear[0]) + "  ·  R: " + Model.ear(root.snapshot.in_ear[1])
            color: Color.popups.text
            opacity: 0.7
            font.family: Style.font.family
            font.pixelSize: Style.font.caption
            wrapMode: Text.Wrap
          }

          Column {
            width: parent.width
            visible: !root.snapshot.configured
            spacing: Style.space(8)
            Text {
              width: parent.width
              textFormat: Text.PlainText
              text: root.snapshot.paired_devices.length
                ? "Select your paired AirPods. Setup connects once to enroll this computer; after that, only in-ear detection permits connection."
                : "Pair your AirPods in Bluetooth first. Paired devices will appear here when the backend is available."
              color: Color.popups.text
              font.family: Style.font.family
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.Wrap
            }
            Ui.Dropdown {
              id: devices
              width: parent.width
              label: "Paired AirPods"
              options: root.deviceOptions
              value: root.selectedAddress
              enabled: root.online && !root.busy
              opacity: enabled ? 1 : 0.45
              onChanged: function(value) { root.selectedAddress = value }
            }
            Ui.Button {
              width: parent.width
              text: root.busy ? root.service.action : "Connect once & set up"
              foreground: Color.popups.text
              bordered: true
              focusable: true
              enabled: root.online && !root.busy && root.selectedAddress !== ""
              opacity: enabled ? 1 : 0.45
              onClicked: root.service.setup(root.selectedAddress)
            }
          }

          Column {
            width: parent.width
            visible: root.snapshot.configured
            spacing: Style.space(8)
            Text {
              textFormat: Text.PlainText
              text: "Listening mode"
              color: Color.popups.text
              font.family: Style.font.family
              font.pixelSize: Style.font.body
              font.bold: true
            }
            Grid {
              width: parent.width
              columns: 2
              spacing: Style.space(6)
              Repeater {
                model: [
                  {value: "off", label: "Off"},
                  {value: "anc", label: "Noise cancellation"},
                  {value: "transparency", label: "Transparency"},
                  {value: "adaptive", label: "Adaptive"}
                ]
                delegate: Ui.Button {
                  required property var modelData
                  width: (parent.width - parent.spacing) / 2
                  text: modelData.label
                  fontSize: Style.font.caption
                  horizontalPadding: Style.space(4)
                  foreground: Color.popups.text
                  selected: root.snapshot.mode === modelData.value
                  bordered: true
                  focusable: true
                  enabled: !!root.service && root.service.canSetMode
                  opacity: enabled ? 1 : 0.45
                  onClicked: root.service.setMode(modelData.value)
                }
              }
            }
            Text {
              width: parent.width
              textFormat: Text.PlainText
              text: root.busy ? root.service.action
                : "Opening the case does not connect. Removing one bud pauses MPRIS playback; putting it back resumes only playback this service paused."
              color: Color.popups.text
              opacity: 0.7
              font.family: Style.font.family
              font.pixelSize: Style.font.caption
              wrapMode: Text.Wrap
            }
          }

          Text {
            width: parent.width
            visible: root.errorText !== ""
            textFormat: Text.PlainText
            text: root.errorText
            color: Color.urgent
            font.family: Style.font.family
            font.pixelSize: Style.font.bodySmall
            wrapMode: Text.Wrap
          }
        }
      }
    }
  }
}
