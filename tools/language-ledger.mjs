#!/usr/bin/env node
// Generate the `projects/language` status ledger: per-crate capability state
// (stub / partial / full) plus the `data.text.*` languages without a service
// crate. Run from the repository root; `--write <file>` writes the markdown
// instead of printing it to stdout.
import { existsSync, mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs'
import { dirname, join, relative } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = fileURLToPath(new URL('..', import.meta.url))
const languageRoot = join(root, 'projects', 'language')
const dataRoot = join(root, 'projects', 'data')

const CAPABILITIES = ['lsp', 'mcp', 'formatter', 'highlighter']
const STUB_LIMIT = 2 * 1024
const PARTIAL_LIMIT = 5 * 1024
// `data.text.*` crates that are shared infrastructure, not languages.
const NON_LANGUAGE = new Set([
  'data.text.core',
  'data.text.css-selector',
  'data.text.pretty',
  'data.text.resolver',
  'data.text.source-map',
  'data.text.symbols',
  'data.text.xpath',
])

function bytesOf(dir) {
  if (!existsSync(dir)) {
    return 0
  }
  let total = 0
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name)
    total += entry.isDirectory() ? bytesOf(path) : statSync(path).size
  }
  return total
}

function stateOf(size) {
  if (size === 0) {
    return '—'
  }
  if (size <= STUB_LIMIT) {
    return 'stub'
  }
  return size <= PARTIAL_LIMIT ? 'partial' : 'full'
}

function listDirs(dir) {
  return readdirSync(dir, { withFileTypes: true })
    .filter((entry) => entry.isDirectory())
    .map((entry) => entry.name)
}

function kb(bytes) {
  return `${(bytes / 1024).toFixed(1)}K`
}

const dataCrates = new Set(listDirs(dataRoot))

const frozen = new Set(JSON.parse(readFileSync(join(root, 'tools', 'language-frozen.json'), 'utf8')).frozen ?? [])


function walkManifests(dir, out = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === 'target' || entry.name === 'node_modules') {
      continue
    }
    const path = join(dir, entry.name)
    if (entry.isDirectory()) {
      walkManifests(path, out)
    } else if (entry.name === 'Cargo.toml') {
      out.push(path)
    }
  }
  return out
}

const DEPENDENCY_SECTION = /(^|\.)(dependencies|dev-dependencies|build-dependencies)$/

function parseManifest(path) {
  const text = readFileSync(path, 'utf8')
  let section = ''
  let name = ''
  const dependencies = new Set()
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim()
    if (!line || line.startsWith('#')) {
      continue
    }
    const header = /^\[([^\]]+)\]$/.exec(line)
    if (header) {
      section = header[1]
      continue
    }
    if (section === 'package') {
      const declared = /^name\s*=\s*"([^"]+)"/.exec(line)
      if (declared) {
        name = declared[1]
      }
      continue
    }
    if (!DEPENDENCY_SECTION.test(section)) {
      continue
    }
    const dependency = /^([A-Za-z0-9_-]+)\s*=/.exec(line)
    if (dependency) {
      dependencies.add(dependency[1])
    }
  }
  return { name, dependencies }
}

const manifests = walkManifests(join(root, 'projects'))
  .map(parseManifest)
  .filter((manifest) => manifest.name.length > 0)


const crates = listDirs(languageRoot)
  .filter((name) => name.startsWith('language.'))
  .map((dir) => {
    const lang = dir.slice('language.'.length)
    const src = join(languageRoot, dir, 'src')
    const capabilities = Object.fromEntries(
      CAPABILITIES.map((capability) => {
        const path = join(src, capability)
        return [capability, { size: bytesOf(path), state: stateOf(bytesOf(path)) }]
      }),
    )
    const srcBytes = bytesOf(src)
    const status = CAPABILITIES.some((capability) => capabilities[capability].state === 'full')
      ? 'full'
      : srcBytes <= STUB_LIMIT
        ? 'stub'
        : 'partial'
    return {
      name: dir,
      kind: dataCrates.has(`data.text.${lang}`) ? 'service' : 'platform',
      srcBytes,
      capabilities,
      tests: existsSync(join(languageRoot, dir, 'tests')),
      computedStatus: status,
      status: frozen.has(dir) ? 'frozen' : status,
    }
  })
  .sort((a, b) => a.kind.localeCompare(b.kind) || a.name.localeCompare(b.name))

const allLanguageCrates = new Set(crates.map((crate) => crate.name.slice('language.'.length)))

// Reverse dependency edges: who actually consumes each language crate.
function consumersOf(crate) {
  const packageName = `language-${crate.name.slice('language.'.length)}`
  const host = []
  const language = []
  const data = []
  for (const manifest of manifests) {
    if (manifest.name === packageName || !manifest.dependencies.has(packageName)) {
      continue
    }
    if (manifest.name.startsWith('language-')) {
      language.push(manifest.name)
    }
    else if (manifest.name.startsWith('data-')) {
      data.push(manifest.name)
    }
    else {
      host.push(manifest.name)
    }
  }
  return { host, language, data }
}

const ranked = crates
  .map((crate) => ({ crate, consumers: consumersOf(crate) }))
  .filter(({ crate, consumers }) =>
    crate.status === 'stub' || crate.status === 'frozen' || consumers.host.length + consumers.language.length + consumers.data.length > 0,
  )
  .sort(
    (a, b) =>
      b.consumers.host.length - a.consumers.host.length ||
      b.consumers.language.length - a.consumers.language.length ||
      a.crate.name.localeCompare(b.crate.name),
  )


const gaps = listDirs(dataRoot)
  .filter((name) => name.startsWith('data.text.'))
  .map((dir) => ({ dir, lang: dir.slice('data.text.'.length) }))
  .filter(({ lang }) => !allLanguageCrates.has(lang))
  .map(({ dir, lang }) => {
    const src = join(dataRoot, dir, 'src')
    const hasParser = existsSync(join(src, 'parser')) || existsSync(join(src, 'lexer'))
    return { dir, note: NON_LANGUAGE.has(dir) ? '非语言件' : '语言缺口', structure: hasParser ? 'lexer/parser' : 'no lexer/parser' }
  })
  .sort((a, b) => a.note.localeCompare(b.note) || a.dir.localeCompare(b.dir))

const summary = {
  total: crates.length,
  service: crates.filter((crate) => crate.kind === 'service').length,
  platform: crates.filter((crate) => crate.kind === 'platform').length,
  lsp: crates.filter((crate) => crate.capabilities.lsp.size > 0).length,
  mcp: crates.filter((crate) => crate.capabilities.mcp.size > 0).length,
  lspAndMcp: crates.filter((crate) => crate.capabilities.lsp.size > 0 && crate.capabilities.mcp.size > 0).length,
  lspFull: crates.filter((crate) => crate.capabilities.lsp.state === 'full').length,
  stub: crates.filter((crate) => crate.computedStatus === 'stub').length,
  frozen: crates.filter((crate) => crate.status === 'frozen').length,
  full: crates.filter((crate) => crate.status === 'full').length,
  tests: crates.filter((crate) => crate.tests).length,
  hostConsumers: crates.filter((crate) => consumersOf(crate).host.length > 0).length,
  gapLanguages: gaps.filter((gap) => gap.note === '语言缺口').length,
  nonLanguages: gaps.filter((gap) => gap.note === '非语言件').length,
}

const lines = [
  '<!-- generated by tools/language-ledger.mjs; do not edit by hand -->',
  '',
  '## 汇总',
  '',
  '| 项 | 数量 |',
  '| --- | --- |',
  `| language crate 总数 | ${summary.total} |`,
  `| 语言服务 crate | ${summary.service} |`,
  `| 平台件 crate | ${summary.platform} |`,
  `| 含 \`src/lsp\` | ${summary.lsp} |`,
  `| 含 \`src/mcp\` | ${summary.mcp} |`,
  `| lsp + mcp 并存 | ${summary.lspAndMcp} |`,
  `| \`src/lsp\` 为 full（>5KB） | ${summary.lspFull} |`,
  `| 整体判为 stub（\`src\` ≤2KB） | ${summary.stub} |`,
  `| 其中已冻结（\`tools/language-frozen.json\`） | ${summary.frozen} |`,
  `| 整体判为 full（任一能力 >5KB） | ${summary.full} |`,
  `| 带 \`tests/\` | ${summary.tests} |`,
  `| 有 host（产品 / CLI / 工具）消费者 | ${summary.hostConsumers} |`,
  `| 无服务 crate 的语言 | ${summary.gapLanguages} |`,
  `| 无服务 crate 的非语言件 | ${summary.nonLanguages} |`,
  '',
  '分层口径：单个能力目录按字节数 `stub ≤2KB` / `partial ≤5KB` / `full >5KB`；',
  'crate 整体状态取「任一能力 full 则 full，否则 `src` ≤2KB 为 stub，其余 partial」；',
  '`frozen` 为 P2 冻结名单（无消费者、无测试，数据侧保留，Gate 5 禁止被依赖）。',
  '`kind=platform` 表示 `projects/data/data.text.<lang>` 不存在，即该 crate 不对应具体语言。',
  '',
  '## crate 明细',
  '',
  '| crate | kind | src | lsp | mcp | formatter | highlighter | tests | status |',
  '| --- | --- | --- | --- | --- | --- | --- | --- | --- |',
  ...crates.map((crate) =>
    [
      crate.name,
      crate.kind,
      kb(crate.srcBytes),
      ...CAPABILITIES.map((capability) => crate.capabilities[capability].state),
      crate.tests ? 'yes' : 'no',
      crate.status,
    ].join(' | ').replace(/^/, '| ').concat(' |'),
  ),
  '',
  '## 消费者排序',
  '',
  '反向依赖按包名前缀分为 `host`（产品 / CLI / 工具，即真实消费者）、`language`（其它语言 crate）、`data`（数据侧，按 Gate 2 应为 0）。',
  '无任何消费者且非 stub 的 crate 不列出；stub crate 始终列出，供 P2 三档处理。',
  '',
  '| crate | status | host | language | data | host 消费者 |',
  '| --- | --- | --- | --- | --- | --- |',
  ...ranked.map(({ crate, consumers }) =>
    [
      crate.name,
      crate.status,
      consumers.host.length,
      consumers.language.length,
      consumers.data.length,
      consumers.host.join('、') || '—',
    ].join(' | ').replace(/^/, '| ').concat(' |'),
  ),
  '',
  '## `data.text.*` 无服务 crate',
  '',
  '| data crate | 判定 | `src` 结构 |',
  '| --- | --- | --- |',
  ...gaps.map((gap) => `| ${gap.dir} | ${gap.note} | ${gap.structure} |`),
  '',
]

const markdown = `${lines.join('\n')}`
const writeIndex = process.argv.indexOf('--write')
if (writeIndex !== -1) {
  const target = process.argv[writeIndex + 1]
  if (!target) {
    console.error('--write needs a target path')
    process.exit(1)
  }
  const path = join(root, target)
  mkdirSync(dirname(path), { recursive: true })
  writeFileSync(path, markdown)
  console.log(`wrote ${relative(root, path)} (${summary.total} crates, ${summary.gapLanguages} language gaps)`)
} else {
  process.stdout.write(markdown)
}
