pragma Singleton
import QtQuick
import Quickshell
import Quickshell.Io

Item {
  id: root
  property var snapshot: null
  readonly property bool online: snapshot !== null && snapshot.brokers > 0

  function count(value) {
    return Number.isSafeInteger(value) && value >= 0
  }

  function refresh() {
    if (!reader.running)
      reader.running = true
  }

  Component.onCompleted: refresh()

  Process {
    id: reader
    command: Quickshell.env("ZRUN_STATS_BIN")
      ? [Quickshell.env("ZRUN_STATS_BIN"), "--stats"]
      : ["sh", "-c", 'if [ -x "$HOME/.local/bin/zrun" ]; then exec "$HOME/.local/bin/zrun" --stats; fi; exec zrun --stats']
    stdout: StdioCollector { id: output }
    onExited: function(code) {
      let report = null
      if (code === 0) {
        try {
          const data = JSON.parse(output.text)
          const counters = data.counters
          if (data.version === 1 && root.count(data.brokers) && root.count(data.avoided)
              && counters && ["started", "memory_hits", "valkey_hits", "coalesced", "active", "queued"]
                .every(key => root.count(counters[key])))
            report = data
        } catch (_) {}
      }
      root.snapshot = report
    }
  }

  Timer {
    interval: 2000
    repeat: true
    running: true
    onTriggered: root.refresh()
  }
}
