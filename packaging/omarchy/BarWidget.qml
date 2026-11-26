import QtQuick
import qs.Commons as Commons
import qs.Ui as Ui

Ui.BarWidget {
  id: root
  moduleName: "lightjunction.zrun"

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  Commons.Translations {
    id: strings
    directory: Qt.resolvedUrl("i18n")
  }

  Ui.BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: "󰆍"
    labelText: !root.vertical && Stats.online
      ? Commons.I18n.formatNumber(Stats.snapshot.counters.started, 0) : ""
    fixedWidth: root.vertical ? -1 : slotSize + (labelText === "" ? 0
      : labelMetrics.advanceWidth + Commons.Style.space(4))
    active: Stats.online && Stats.snapshot.counters.active > 0
    tooltipText: !Stats.snapshot ? strings.t("unavailable") : Stats.online
      ? strings.t("summary")
          .arg(Commons.I18n.formatNumber(Stats.snapshot.counters.started, 0))
          .arg(Commons.I18n.formatNumber(Stats.snapshot.avoided, 0))
      : strings.t("waiting")

    TextMetrics {
      id: labelMetrics
      font.family: button.fontFamily
      font.pixelSize: button.fontSize
      text: button.labelText
    }

    onPressed: function(mouseButton) {
      if (mouseButton === Qt.LeftButton && root.bar && root.bar.shell)
        root.bar.shell.toggle(root.moduleName, "")
    }
  }
}
