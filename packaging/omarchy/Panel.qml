import QtQuick
import QtQuick.Layouts
import qs.Commons as Commons

ColumnLayout {
  id: root
  spacing: Commons.Style.space(8)
  readonly property var counters: Stats.online ? Stats.snapshot.counters : null

  Commons.Translations {
    id: strings
    directory: Qt.resolvedUrl("i18n")
  }

  component Label: Text {
    color: Commons.Color.popups.text
    font.family: Commons.Style.font.menuFamily
    font.pixelSize: Commons.Style.font.body
    textFormat: Text.PlainText
  }

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
        color: modelData.key === "avoided" ? Commons.Color.accent : Commons.Color.popups.text
      }
    }
  }
}
