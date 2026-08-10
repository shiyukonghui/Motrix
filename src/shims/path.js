// Browser-safe minimal `path` polyfill used by the web build.
// The Tauri WebView has no Node.js `path` module, but some bundled
// dependencies (e.g. parse-torrent) still import it. This implements the
// common subset with POSIX semantics, which is enough for parsing torrents.
const sep = '/'
const delimiter = ':'

const normalize = (p) => {
  const parts = String(p || '').replace(/\\/g, '/').split('/')
  const result = []
  parts.forEach((part) => {
    if (part === '' || part === '.') return
    if (part === '..') {
      result.pop()
    } else {
      result.push(part)
    }
  })
  const joined = result.join('/')
  return (String(p).startsWith('/') ? '/' : '') + joined || '.'
}

const join = (...args) => {
  const parts = args.filter((item) => typeof item === 'string' && item.length > 0)
  if (parts.length === 0) {
    return '.'
  }
  return normalize(parts.join('/'))
}

const resolve = (...args) => {
  const parts = args.filter((item) => typeof item === 'string' && item.length > 0)
  return join('/', ...parts)
}

const basename = (p, ext) => {
  const base = String(p || '').replace(/\\/g, '/').split('/').filter(Boolean).pop() || ''
  if (ext && base.endsWith(ext)) {
    return base.slice(0, -ext.length)
  }
  return base
}

const extname = (p) => {
  const base = basename(p)
  const index = base.lastIndexOf('.')
  return index > 0 ? base.slice(index) : ''
}

const dirname = (p) => {
  const parts = String(p || '').replace(/\\/g, '/').split('/')
  parts.pop()
  return parts.join('/') || (String(p).startsWith('/') ? '/' : '.')
}

const isAbsolute = (p) => {
  return typeof p === 'string' && p.startsWith('/')
}

export const path = {
  sep,
  delimiter,
  normalize,
  join,
  resolve,
  basename,
  extname,
  dirname,
  isAbsolute,
  posix: {
    sep,
    delimiter,
    normalize,
    join,
    resolve,
    basename,
    extname,
    dirname,
    isAbsolute
  }
}

export default path
