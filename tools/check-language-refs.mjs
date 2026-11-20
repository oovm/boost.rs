#!/usr/bin/env node
// Gate: keep the data/language boundary one-directional.
//
//   1. `projects/data/data.text.*/src` must not contain service modules
//      (lsp / mcp / formatter / highlighter).
//   2. `data.text.*` manifests must not depend on a `language-*` service crate.
//   3. `language.*` manifests must not reuse another language's `data-text-*`
//      implementation; only the shared basics and the crate's own data crate
//      are allowed.
//   4. `language.*/src/mcp` is a protocol adapter: it must reuse the same
//      language service as `src/lsp`, not carry its own semantics or stay empty.
//
// Run from the repository root.
import { execFileSync } from 'node:child_process'
import { existsSync, readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = fileURLToPath(new URL('..', import.meta.url))
const tracked = execFileSync('git', ['ls-files', '-z'], { cwd: root, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 })
  .split('\0')
  .filter(Boolean)

const SERVICE_DIRS = ['lsp', 'mcp', 'formatter', 'highlighter']
// Shared data-side pieces every language crate may use. `data-text-von` is the
// serialization model owned by `data.text`, not a language service crate.
const BASIC_DATA_DEPS = new Set(['data-text-core', 'data-text-pretty', 'data-text-symbols', 'data-text-resolver', 'data-text-von'])

const languageCrates = new Set()
const dataCrates = new Set()
for (const file of tracked) {
  const language = file.match(/^projects\/language\/(language\.[^/]+)\//)
  if (language) {
    languageCrates.add(language[1])
  }
  const data = file.match(/^projects\/data\/(data\.text\.[^/]+)\//)
  if (data) {
    dataCrates.add(data[1])
  }
}

const failures = []

function report(file, line, message) {
  failures.push(`${file}:${line}: ${message}`)
}

// ---- Gate 1: no service modules on the data side -------------------------
for (const file of tracked) {
  const match = file.match(/^projects\/data\/(data\.text\.[^/]+)\/src\/(lsp|mcp|formatter|highlighter)\//)
  if (match) {
    report(file, 1, `service module "${match[2]}" belongs to language.${match[1].slice('data.text.'.length)}, not the data crate`)
  }
}

// ---- Manifest parsing ----------------------------------------------------
function dependenciesOf(file) {
  const lines = readFileSync(resolve(root, file), 'utf8').split(/\r?\n/)
  const dependencies = []
  let section = ''
  lines.forEach((line, index) => {
    const header = line.match(/^\s*\[([^\]]+)\]/)
    if (header) {
      section = header[1].trim()
      const table = section.match(/^(?:target\..+\.)?(?:dev-|build-)?dependencies\.([A-Za-z0-9_.-]+)$/)
      if (table) {
        dependencies.push({ name: table[1], section, line: index + 1 })
      }
      return
    }
    if (!/(?:^|\.)(?:dependencies|dev-dependencies|build-dependencies)$/.test(section)) {
      return
    }
    const entry = line.match(/^\s*([A-Za-z0-9_.-]+)\s*=/)
    if (entry) {
      dependencies.push({ name: entry[1], section, line: index + 1 })
    }
  })
  return dependencies
}

// ---- Gate 2: data crates must not depend on language service crates ------
for (const dataCrate of dataCrates) {
  const file = `projects/data/${dataCrate}/Cargo.toml`
  if (!tracked.includes(file)) {
    continue
  }
  for (const dependency of dependenciesOf(file)) {
    const language = dependency.name.match(/^language-(.+)$/)
    if (!language) {
      continue
    }
    if (dataCrates.has(`data.text.${language[1]}`)) {
      report(file, dependency.line, `data crate must not depend on language service crate "${dependency.name}"`)
    }
  }
}

// ---- Gate 3: language crates may only reuse their own data crate ---------
for (const languageCrate of languageCrates) {
  const file = `projects/language/${languageCrate}/Cargo.toml`
  if (!tracked.includes(file)) {
    continue
  }
  const lang = languageCrate.slice('language.'.length)
  for (const dependency of dependenciesOf(file)) {
    const data = dependency.name.match(/^data-text-(.+)$/)
    if (!data) {
      continue
    }
    if (BASIC_DATA_DEPS.has(dependency.name) || data[1] === lang) {
      continue
    }
    report(file, dependency.line, `language crate must not reuse another language's data crate "${dependency.name}"`)
  }
}

// ---- Gate 4: mcp is a protocol adapter over the lsp service --------------
for (const file of tracked) {
  if (!/^projects\/language\/language\.[^/]+\/src\/mcp\/mod\.rs$/.test(file) || !existsSync(resolve(root, file))) {
    continue
  }
  const content = readFileSync(resolve(root, file), 'utf8')
  if (!/\bcrate::lsp\b|\bsuper::lsp\b/.test(content)) {
    report(file, 1, 'mcp module must reuse the language service from `crate::lsp` instead of owning semantics')
  }
}

if (failures.length > 0) {
  console.error('language boundary violations:')
  for (const failure of failures) {
    console.error(`  ${failure}`)
  }
  process.exit(1)
}

console.log(
  `checked ${languageCrates.size} language crates and ${dataCrates.size} data crates: ` +
    'no service modules on the data side, no reversed or cross-language dependencies, ' +
    'every mcp adapter reuses its lsp service',
)
