// shell shim replacing `@electron/remote`'s shell module.
// The actual platform commands are implemented as Tauri commands
// (`show_item_in_folder` / `open_path` / `trash_item`) on the Rust side;
// failures are ignored so the UI keeps working during the migration.
import { invoke } from '@tauri-apps/api/core'

const safeInvoke = (command, payload) => {
  return invoke(command, payload).catch((err) => {
    console.warn(`[Motrix] shim invoke "${command}" failed:`, err)
  })
}

export const shell = {
  showItemInFolder (fullPath) {
    return safeInvoke('show_item_in_folder', { path: fullPath })
  },
  openPath (fullPath) {
    return safeInvoke('open_path', { path: fullPath })
  },
  trashItem (fullPath) {
    return safeInvoke('trash_item', { path: fullPath })
  }
}

export default shell
