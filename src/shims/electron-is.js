// electron-is shim for Tauri.
// In Tauri the renderer always runs in the renderer process, so `is.renderer()`
// is always true. The OS platform is injected by the Rust side as
// `window.__TAURI_OS_PLATFORM__` (win32 / darwin / linux), and we fall back to
// the user agent when the injection is not present yet.

const getPlatform = () => {
  if (typeof window !== 'undefined' && window.__TAURI_OS_PLATFORM__) {
    return window.__TAURI_OS_PLATFORM__
  }

  const ua = (typeof navigator !== 'undefined' && navigator.userAgent) || ''
  if (ua.includes('Windows')) {
    return 'win32'
  }
  if (ua.includes('Mac')) {
    return 'darwin'
  }
  return 'linux'
}

const isDev = () => {
  const { port } = (typeof window !== 'undefined' && window.location) || {}
  // 1420: tauri dev server; 9080: webpack dev server
  return port === '1420' || port === '9080'
}

export const is = {
  renderer: () => true,
  main: () => false,
  macOS: () => getPlatform() === 'darwin',
  windows: () => getPlatform() === 'win32',
  linux: () => getPlatform() === 'linux',
  mas: () => false,
  dev: isDev
}

export default is
