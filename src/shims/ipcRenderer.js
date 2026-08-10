// ipcRenderer shim: maps the Electron IPC channels used by the renderer to
// Tauri events / commands.
//
//   send('command', name, ...args)      -> emit('command', { command, args })
//   send('event', name, ...args)        -> emit('event', { eventName, args })
//   on('command', listener)             -> listen('command:dispatch')
//   on(channel, listener)               -> listen(channel)
//   invoke('get-app-config')            -> invoke('get_app_config')
import { invoke as tauriInvoke } from '@tauri-apps/api/core'
import { listen, emit } from '@tauri-apps/api/event'

// channel -> Array<{ listener, unlisten }>
const unlistenRegistry = new Map()

const register = (channel, listener, unlisten) => {
  const list = unlistenRegistry.get(channel) || []
  list.push({ listener, unlisten })
  unlistenRegistry.set(channel, list)
}

export const ipcRenderer = {
  send (channel, ...args) {
    if (channel === 'command') {
      const [command, ...rest] = args
      return emit('command', { command, args: rest })
    }
    if (channel === 'event') {
      const [eventName, ...rest] = args
      return emit('event', { eventName, args: rest })
    }
    return emit(channel, ...args)
  },

  on (channel, listener) {
    if (channel === 'command') {
      return listen('command:dispatch', (e) => {
        listener(e, e.payload.command, ...(e.payload.args || []))
      }).then((unlisten) => {
        register(channel, listener, unlisten)
        return unlisten
      })
    }
    return listen(channel, (e) => {
      listener(e, ...(e.payload || []))
    }).then((unlisten) => {
      register(channel, listener, unlisten)
      return unlisten
    })
  },

  removeListener (channel, listener) {
    const list = unlistenRegistry.get(channel) || []
    const rest = []
    list.forEach((item) => {
      if (item.listener === listener && item.unlisten) {
        item.unlisten()
      } else {
        rest.push(item)
      }
    })
    unlistenRegistry.set(channel, rest)
  },

  removeAllListeners (channel) {
    if (channel) {
      const list = unlistenRegistry.get(channel) || []
      list.forEach(({ unlisten }) => {
        if (unlisten) {
          unlisten()
        }
      })
      unlistenRegistry.delete(channel)
    } else {
      unlistenRegistry.forEach((list) => {
        list.forEach(({ unlisten }) => {
          if (unlisten) {
            unlisten()
          }
        })
      })
      unlistenRegistry.clear()
    }
  },

  invoke (channel, ...args) {
    if (channel === 'get-app-config') {
      return tauriInvoke('get_app_config')
    }
    return tauriInvoke(channel, ...args)
  }
}

export default ipcRenderer
