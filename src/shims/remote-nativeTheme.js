// nativeTheme shim replacing `@electron/remote`'s nativeTheme.
// In Tauri the WebView follows the OS theme, so we read it directly from the
// `prefers-color-scheme` media query instead of going through a command.
export const nativeTheme = {
  get shouldUseDarkColors () {
    return typeof window !== 'undefined' &&
      window.matchMedia &&
      window.matchMedia('(prefers-color-scheme: dark)').matches
  }
}

export default nativeTheme
