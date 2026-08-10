// Unified entry for the Tauri shims.
// Installs `$electron` on Vue.prototype (replacing `vue-electron`) and exposes
// every shim object so the renderer code can import them directly.
import { is } from './electron-is'
import { ipcRenderer } from './ipcRenderer'
import { shell } from './remote-shell'
import { nativeTheme } from './remote-nativeTheme'
import { dialog, app, webContents, getCurrentWindow } from './remote'

export function install (Vue) {
  Vue.prototype.$electron = { ipcRenderer }
  window.__shims__ = { is, ipcRenderer, shell, nativeTheme }
}

export {
  is,
  ipcRenderer,
  shell,
  nativeTheme,
  dialog,
  app,
  webContents,
  getCurrentWindow
}

export default is
