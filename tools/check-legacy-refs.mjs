#!/usr/bin/env node
// Gate: block any new reference to the retired Oak/Acorn packages, git sources,
// patch roots, or import paths. Run from the repository root.
import { execFileSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = fileURLToPath(new URL('..', import.meta.url))
const tracked = execFileSync('git', ['ls-files', '-z'], { cwd: root, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 })
  .split('\0')
  .filter(Boolean)

const manifestRules = [
  ['legacy git source', /yggdrasil-language/i],
  ['legacy package rename', /^\s*package\s*=\s*"(?:oak|acorn)[-_.]/im],
  ['legacy dependency key', /^\s*"?(?:oak|acorn)[-_.][A-Za-z0-9_.-]*"?\s*=/im],
]

const lockRules = [
  ['legacy package in lock', /^name\s*=\s*"(?:oak|acorn)[-_.]/im],
  ['legacy git source in lock', /yggdrasil-language/i],
]

const sourceRule =
  /\b(?:oak|acorn)_(?:core|macros|formats|lsp|mcp|highlight|pretty_print|formatter|repl|folding|hover|navigation|semantic_tokens|semantic_search|structural_view|symbols|resolver|source_map|testing|visualize|codec|asn1|probe|ttf|zip|tar|source|host|aarch64|x64|tvm|odin|mach_o|pe|cfb|doc|docx|epub|jvm|dex|wasm|spirv|dxbc|hsaco)\b|\b(?:extern\s+crate|use)\s+(?:oak|acorn)\b/g

const failures = []

function scan(file, rules) {
  const text = readFileSync(resolve(root, file), 'utf8')
  for (const [name, rule] of rules) {
    rule.lastIndex = 0
    let match
    while ((match = rule.exec(text)) !== null) {
      failures.push(`${file}:${lineOf(text, match.index)}: ${name}: ${match[0].trim()}`)
    }
  }
}

function scanSource(file) {
  const text = readFileSync(resolve(root, file), 'utf8')
  const lines = text.split(/\r?\n/)
  lines.forEach((line, index) => {
    if (/^\s*(\/\/|\/\*|\*)/.test(line)) {
      return
    }
    sourceRule.lastIndex = 0
    let match
    while ((match = sourceRule.exec(line)) !== null) {
      failures.push(`${file}:${index + 1}: legacy import path: ${match[0]}`)
    }
  })
}

function lineOf(text, offset) {
  return text.slice(0, offset).split('\n').length
}

for (const file of tracked) {
  if (file === 'Cargo.toml' || file.endsWith('/Cargo.toml')) {
    scan(file, manifestRules)
  } else if (file === 'Cargo.lock') {
    scan(file, lockRules)
  } else if (file.endsWith('.cargo/config.toml') || file.endsWith('.cargo/config')) {
    scan(file, [['legacy patch source', /yggdrasil-language/i]])
  } else if (file.endsWith('.rs')) {
    scanSource(file)
  }
}

if (failures.length > 0) {
  console.error('legacy Oak/Acorn references are not allowed:')
  for (const failure of failures) {
    console.error(`  ${failure}`)
  }
  process.exit(1)
}

console.log(`checked ${tracked.length} tracked files: no legacy Oak/Acorn references`)
