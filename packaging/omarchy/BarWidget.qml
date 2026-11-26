import QtQuick
import qs.Commons as Commons
import qs.Ui as Ui

Ui.BarWidget {
  id: root
  moduleName: "lightjunction.zrun"

  function close() { popup.open = false }

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
      if (mouseButton === Qt.LeftButton) {
        popup.open = !popup.open
        if (popup.open)
          Stats.refresh()
      }
    }
  }

  Ui.PopupCard {
    id: popup
    anchorItem: button
    owner: root
    bar: root.bar
    contentWidth: fittedContentWidth(Commons.Style.space(320))
    contentHeight: fittedContentHeight(details.implicitHeight)

    Panel {
      id: details
      anchors.left: parent.left
      anchors.right: parent.right
    }
  }
}
