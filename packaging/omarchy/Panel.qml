import QtQuick
import QtQuick.Layouts
import qs.Commons as Commons

ColumnLayout {
  id: root

  readonly property var counters: Stats.online ? Stats.snapshot.counters : null
  readonly property color ink: Commons.Color.popups.text
  readonly property color accent: Commons.Color.accent
  readonly property color surface: Commons.Color.popups.background
  readonly property color divider: Qt.rgba(ink.r, ink.g, ink.b, 0.16)

  spacing: Commons.Style.space(12)

  function number(value) { return Commons.I18n.formatNumber(value, 0) }

  function tint(color, amount) {
    return Qt.rgba(surface.r + (color.r - surface.r) * amount,
                   surface.g + (color.g - surface.g) * amount,
                   surface.b + (color.b - surface.b) * amount, 1)
  }

  Commons.Translations {
    id: strings
    directory: Qt.resolvedUrl("i18n")
  }

  component Label: Text {
    color: root.ink
    font.family: Commons.Style.font.menuFamily
    font.pixelSize: Commons.Style.font.body
    textFormat: Text.PlainText
  }

  component Metric: Rectangle {
    id: metric
    required property string label
    required property double value
    property bool highlighted: false
    readonly property int inset: Commons.Style.space(10)

    Layout.fillWidth: true
    Layout.preferredWidth: 1
    implicitHeight: metricText.implicitHeight + inset * 2
    radius: Commons.Style.space(8)
    color: root.tint(highlighted ? root.accent : root.ink, highlighted ? 0.13 : 0.08)

    ColumnLayout {
      id: metricText
      anchors.left: parent.left
      anchors.right: parent.right
      anchors.top: parent.top
      anchors.margins: metric.inset
      spacing: Commons.Style.space(4)

      Label {
        Layout.fillWidth: true
        text: root.number(metric.value)
        color: metric.highlighted ? root.accent : root.ink
        font.pixelSize: Commons.Style.font.body * 2
        font.bold: true
        fontSizeMode: Text.HorizontalFit
        minimumPixelSize: Commons.Style.font.body
      }

      Label {
        Layout.fillWidth: true
        text: metric.label
        font.pixelSize: Commons.Style.font.bodySmall
        wrapMode: Text.Wrap
      }
    }
  }

  RowLayout {
    Layout.fillWidth: true
    spacing: Commons.Style.space(6)

    Label {
      Layout.fillWidth: true
      text: "Zrun"
      font.pixelSize: Commons.Style.font.body + 4
      font.bold: true
    }

    Rectangle {
      visible: Stats.online
      width: Commons.Style.space(6)
      height: width
      radius: width / 2
      color: root.accent
    }

    Label {
      visible: Stats.online
      text: Stats.online ? strings.t("brokers").arg(root.number(Stats.snapshot.brokers)) : ""
      font.pixelSize: Commons.Style.font.bodySmall
      opacity: 0.78
    }
  }

  Label {
    visible: !Stats.online
    Layout.fillWidth: true
    wrapMode: Text.Wrap
    opacity: 0.78
    text: strings.t(Stats.snapshot ? "waiting" : "unavailable")
  }

  RowLayout {
    visible: Stats.online
    Layout.fillWidth: true
    spacing: Commons.Style.space(8)

    Metric {
      label: strings.t("started")
      value: root.counters ? root.counters.started : 0
    }

    Metric {
      label: strings.t("avoided")
      value: Stats.online ? Stats.snapshot.avoided : 0
      highlighted: true
    }
  }

  ColumnLayout {
    visible: Stats.online
    Layout.fillWidth: true
    spacing: Commons.Style.space(5)

    Repeater {
      model: root.counters ? [
        { key: "memory_hits", value: root.counters.memory_hits },
        { key: "valkey_hits", value: root.counters.valkey_hits },
        { key: "coalesced", value: root.counters.coalesced }
      ] : []

      delegate: RowLayout {
        required property var modelData
        Layout.fillWidth: true

        Label {
          Layout.fillWidth: true
          text: strings.t(modelData.key)
          opacity: 0.82
        }

        Label {
          text: root.number(modelData.value)
          opacity: modelData.value > 0 ? 1 : 0.55
        }
      }
    }
  }

  Rectangle {
    visible: Stats.online
    Layout.fillWidth: true
    Layout.preferredHeight: 1
    color: root.divider
  }

  RowLayout {
    visible: Stats.online
    Layout.fillWidth: true

    Label {
      Layout.fillWidth: true
      wrapMode: Text.Wrap
      text: strings.t("scope")
      font.pixelSize: Commons.Style.font.caption
      opacity: 0.72
    }

    Label {
      text: root.counters ? strings.t("activity").arg(root.number(root.counters.active))
        .arg(root.number(root.counters.queued)) : ""
      font.pixelSize: Commons.Style.font.caption
      opacity: root.counters && (root.counters.active || root.counters.queued) ? 1 : 0.72
    }
  }
}
