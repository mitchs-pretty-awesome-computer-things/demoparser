// Export-parity guard for the tracked prebuilt binaries (e.g.
// `demoparser2.linux-x64-gnu.node`).
//
// `index.js` destructures its API from the native binding, so a stale
// tracked binary (built before a `#[napi]` export was added) yields
// `undefined` and every call throws `TypeError`. This check fails if any
// name destructured from `nativeBinding` in `index.js` is missing from a
// tracked `.node` file present in this directory.
//
// Run: `node check-exports.mjs` (from `src/node/`). Exit non-zero lists
// the missing names per binary.
import { readdirSync, readFileSync } from 'fs'
import { join, dirname } from 'path'
import { fileURLToPath } from 'url'
import { createRequire } from 'module'

const require = createRequire(import.meta.url)
const dir = dirname(fileURLToPath(import.meta.url))

// The single `const { ... } = nativeBinding` destructure in index.js.
const indexSrc = readFileSync(join(dir, 'index.js'), 'utf8')
const m = indexSrc.match(/const\s*\{([^}]+)\}\s*=\s*nativeBinding/)
if (!m) {
  console.error('check-exports: no `= nativeBinding` destructure found in index.js')
  process.exit(2)
}
const wanted = m[1].split(',').map((s) => s.trim()).filter(Boolean)
if (wanted.length === 0) {
  console.error('check-exports: empty destructure list in index.js')
  process.exit(2)
}

const binaries = readdirSync(dir).filter((f) => f.endsWith('.node'))
if (binaries.length === 0) {
  console.error('check-exports: no tracked `.node` binaries found')
  process.exit(2)
}

let failed = false
for (const file of binaries) {
  let binding
  try {
    binding = require(join(dir, file))
  } catch (e) {
    // A binary for another platform/arch cannot load here; skip it rather
    // than fail — CI/dev machines only ever load their own triple.
    console.log(`check-exports: skipping unloadable ${file} (${e.code || e.message})`)
    continue
  }
  const missing = wanted.filter((name) => typeof binding[name] === 'undefined')
  if (missing.length > 0) {
    console.error(`check-exports: ${file} is missing exports: ${missing.join(', ')}`)
    failed = true
  } else {
    console.log(`check-exports: ${file} OK (${wanted.length} exports)`)
  }
}
process.exit(failed ? 1 : 0)
