import { useEffect } from "react";
import { homeDir } from "@tauri-apps/api/path";
import { ConnectionsScreen } from "./connections/ConnectionsScreen";
import { setHomeDir } from "./connections/profile";
import { CommandPaletteHost } from "./palette/CommandPalette";
import { useWindowEvents } from "./shell/useWindowEvents";
import { Workspace } from "./shell/Workspace";
import { startTabPersistence } from "./state/persist";
import { useApp } from "./state/store";

function App() {
  const screen = useApp((s) => s.screen);
  const loadConnections = useApp((s) => s.loadConnections);

  useWindowEvents();

  useEffect(() => startTabPersistence(), []);

  // For showing file paths with `~`. Not available in a plain browser (`?mock`) or in tests.
  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return;
    homeDir().then(setHomeDir).catch(() => {});
  }, []);

  useEffect(() => {
    loadConnections().catch(() => {});
  }, [loadConnections]);

  return (
    <main>
      {screen === "connections" ? <ConnectionsScreen /> : <Workspace />}
      <CommandPaletteHost />
    </main>
  );
}

export default App;
