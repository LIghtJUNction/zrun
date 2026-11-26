import QtQuick
import QtQuick.Layouts
import qs.Commons as Commons
import qs.Ui as Ui

Item {
  id: root

  property var shell: null
  property var manifest: null
  property bool opened: false
  readonly property var counters: Stats.online ? Stats.snapshot.counters : null

  function open() {
    Stats.refresh()
    opened = true
  }

  function close() { opened = false }

  function dismiss() {
    if (shell)
      shell.hide("lightjunction.zrun")
    else
      close()
  }

  Commons.Translations {
    id: strings
    directory: Qt.resolvedUrl("i18n")
  }

  component Label: Text {
    color: Commons.Color.menu.text
    font.family: Commons.Style.font.menuFamily
    font.pixelSize: Commons.Style.font.body
    textFormat: Text.PlainText
  }

  Ui.OverlayWindow {
    id: overlay
    shown: root.opened
    onContentRevealedChanged: if (contentRevealed) card.forceActiveFocus()

    Rectangle {
      anchors.fill: parent
      color: Commons.Color.menu.scrim
      MouseArea {
        anchors.fill: parent
        onClicked: root.dismiss()
      }
    }

    Rectangle {
      id: card
      anchors.centerIn: parent
      width: Math.min(460, overlay.width - 32)
      height: content.implicitHeight + 40
      radius: Commons.Style.cornerRadius
      readonly property color surface: Commons.Color.menu.background
      color: Qt.rgba(surface.r, surface.g, surface.b, 1)
      Keys.onEscapePressed: root.dismiss()

      MouseArea { anchors.fill: parent }

      ColumnLayout {
        id: content
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        anchors.margins: 20
        spacing: 12

        Label {
          text: "Zrun"
          font.pixelSize: Commons.Style.font.body + 6
          font.bold: true
        }

        Label {
          Layout.fillWidth: true
          wrapMode: Text.Wrap
          opacity: 0.78
          text: !Stats.snapshot ? strings.t("unavailable") : Stats.online
            ? strings.t("brokers").arg(Commons.I18n.formatNumber(Stats.snapshot.brokers, 0))
            : strings.t("waiting")
        }

        Repeater {
          model: root.counters ? [
            { key: "started", value: root.counters.started },
            { key: "avoided", value: Stats.snapshot.avoided },
            { key: "memory_hits", value: root.counters.memory_hits },
            { key: "valkey_hits", value: root.counters.valkey_hits },
            { key: "coalesced", value: root.counters.coalesced },
            { key: "active", value: root.counters.active },
            { key: "queued", value: root.counters.queued }
          ] : []

          delegate: RowLayout {
            required property var modelData
            Layout.fillWidth: true
            Label {
              Layout.fillWidth: true
              text: strings.t(modelData.key)
              font.bold: modelData.key === "started" || modelData.key === "avoided"
            }
            Label {
              text: Commons.I18n.formatNumber(modelData.value, 0)
              color: modelData.key === "avoided" ? Commons.Color.accent : Commons.Color.menu.text
            }
          }
        }
      }
    }
  }
}
