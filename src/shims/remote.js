// Placeholder shims for the remaining `@electron/remote` APIs used by the
// renderer (`dialog`, `app`, `webContents`, `getCurrentWindow`).
//
// The real Tauri implementations (e.g. `tauri-plugin-dialog`) are wired up in
// later migration tasks; until then these safe placeholders keep the web build
// and the app running without errors.
import { getCurrentWindow as tauriGetCurrentWindow } from '@tauri-apps/api/window'

const warn = (api) => {
  console.warn(`[Motrix] @electron/remote shim: "${api}" is not implemented in Tauri yet`)
}

// dialog
export const dialog = {
  showOpenDialog () {
    warn('dialog.showOpenDialog')
    return Promise.resolve({ canceled: true, filePaths: [] })
  },
  showMessageBox () {
    warn('dialog.showMessageBox')
    return Promise.resolve({ response: 0, checkboxChecked: false })
  }
}

// app
const getConfig = () => {
  const root = (typeof global !== 'undefined' && global.app) || null
  const config = root &&
    root.$store &&
    root.$store.state &&
    root.$store.state.preference &&
    root.$store.state.preference.config
  return config || {}
}

export const app = {
  getVersion () {
    const { version } = getConfig()
    return version || ''
  }
}

// webContents (only `fromId(...).setWindowOpenHandler` is used)
export const webContents = {
  fromId () {
    return {
      setWindowOpenHandler () {}
    }
  }
}

// window controls used by the native title bar
let isMaximized = false

const callWindow = (fn) => {
  try {
    const result = fn()
    if (result && typeof result.catch === 'function') {
      result.catch(() => {})
    }
  } catch (err) {
    console.warn('[Motrix] window shim call failed:', err)
  }
}

export const getCurrentWindow = () => ({
  minimize: () => callWindow(() => tauriGetCurrentWindow().minimize()),
  maximize: () => {
    isMaximized = true
    callWindow(() => tauriGetCurrentWindow().maximize())
  },
  unmaximize: () => {
    isMaximized = false
    callWindow(() => tauriGetCurrentWindow().unmaximize())
  },
  isMaximized: () => isMaximized,
  close: () => callWindow(() => tauriGetCurrentWindow().close())
})
