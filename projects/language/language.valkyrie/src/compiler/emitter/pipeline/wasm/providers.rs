//! Valkyrie × Wasm/JS 宿主导入 provider：语言 std 文本行为由适配器显式登记。
//!
//! 架构裁定（D5）：`Utf8Text` / `Utf16Text` 的宿主侧语义属于语言，不属于
//! 目标 lane。此前 JS glue 在生成壳时硬编码了整套文本实现（`env::utf8_*` /
//! `env::utf16_*`），等于把语言 std 行为内建进目标编码层。现在这些行为以
//! [`WasmHostImportProviderFact`] 的形式由本模块（语言 × 目标适配器）提交，
//! 经 `nyar.host_providers` 自定义段交给 JS 启动壳注入；glue 只保留通用
//! 装载/编组（句柄表、字面量表、cabi 边界、CLI/文件能力）。
//!
//! 行为合同逐字保留：utf8 按 Unicode scalar（`[...s]`），utf16 按 UTF-16
//! code unit；`White_Space` 集合与 std `trim` 一致。文本句柄与宿主字符串
//! 表的互操作依赖启动壳的 `hostPathFromRef` / `hostIntern`（通用装载层）。

use compiler_emitter::emitter_backend_wasm::{WasmAbiCarrier, WasmHostImportProviderFact};

/// 共享辅助 JS：文本句柄读写、scalar/code-unit 索引合同与 `White_Space` 集合。
///
/// 注入一次（挂在首个 provider 条目上），全部文本操作实现共享同一份定义。
const TEXT_HELPERS: &str = r#"// Valkyrie std 文本宿主行为（语言 × 目标适配器 provider，JS 壳不内建）。
//
// env.utf8_* index contract (matches std Utf8Text): Unicode **scalars** via [...s],
// never JS string.length / indexOf UTF-16 code units. Host JS strings are UTF-16 storage only.
function hostUtf8Scalars(s) {
    return [...hostPathFromRef(s)];
}

function hostUtf8Length(s) {
    return hostUtf8Scalars(s).length;
}

// Unicode `White_Space` (25 code points), the same set std `Utf8Text::trim` uses.
// `String.prototype.trim` is not equivalent: it strips U+FEFF and keeps U+0085.
const UTF8_WHITE_SPACE = new Set([
    0x0009, 0x000a, 0x000b, 0x000c, 0x000d, 0x0020, 0x0085, 0x00a0, 0x1680,
    0x2000, 0x2001, 0x2002, 0x2003, 0x2004, 0x2005, 0x2006, 0x2007, 0x2008, 0x2009, 0x200a,
    0x2028, 0x2029, 0x202f, 0x205f, 0x3000,
]);

function hostUtf8IsWhiteSpace(scalar) {
    return UTF8_WHITE_SPACE.has(scalar.codePointAt(0));
}

function hostUtf8Trim(s) {
    const scalars = hostUtf8Scalars(s);
    let start = 0;
    let end = scalars.length;
    while (start < end && hostUtf8IsWhiteSpace(scalars[start])) start += 1;
    while (end > start && hostUtf8IsWhiteSpace(scalars[end - 1])) end -= 1;
    return scalars.slice(start, end).join("");
}

function hostUtf8ToLower(s) {
    return hostPathFromRef(s).toLowerCase();
}

function hostUtf8ToUpper(s) {
    return hostPathFromRef(s).toUpperCase();
}

function hostUtf8Concat(a, b) {
    return hostIntern(hostPathFromRef(a) + hostPathFromRef(b));
}

function hostUtf8Replace(s, oldValue, newValue) {
    const text = hostPathFromRef(s);
    const needle = hostPathFromRef(oldValue);
    const replacement = hostPathFromRef(newValue);
    if (hostUtf8Scalars(needle).length === 0) return text;
    return text.split(needle).join(replacement);
}

function hostUtf8StartsWith(s, prefix) {
    return hostPathFromRef(s).startsWith(hostPathFromRef(prefix)) ? 1 : 0;
}

function hostUtf8EndsWith(s, suffix) {
    return hostPathFromRef(s).endsWith(hostPathFromRef(suffix)) ? 1 : 0;
}

function hostUtf8Contains(s, other) {
    return hostPathFromRef(s).includes(hostPathFromRef(other)) ? 1 : 0;
}

function hostUtf8Equals(a, b) {
    return hostPathFromRef(a) === hostPathFromRef(b) ? 1 : 0;
}

function hostUtf8IndexOf(s, other) {
    const chars = hostUtf8Scalars(s);
    const needle = hostUtf8Scalars(other);
    if (needle.length === 0) return 0;
    if (needle.length > chars.length) return -1;
    for (let i = 0; i <= chars.length - needle.length; i++) {
        let ok = true;
        for (let j = 0; j < needle.length; j++) {
            if (chars[i + j] !== needle[j]) { ok = false; break; }
        }
        if (ok) return i;
    }
    return -1;
}

function hostUtf8Slice(s, start, count) {
    const chars = hostUtf8Scalars(s);
    const from = Math.max(0, start | 0);
    const len = Math.max(0, count | 0);
    return chars.slice(from, from + len).join("");
}

// env.utf16_* index contract (matches std Utf16Text): **UTF-16 code units**, i.e.
// native JS string indexing/`.length`. No scalar iteration: a lone surrogate
// stays one code unit and must not be replaced by U+FFFD. This is exactly what
// the utf8 helpers above would lose if they were reused here.
function hostUtf16Text(s) {
    return hostPathFromRef(s);
}

function hostUtf16Length(s) {
    return hostUtf16Text(s).length;
}

function hostUtf16Concat(a, b) {
    return hostIntern(hostUtf16Text(a) + hostUtf16Text(b));
}

function hostUtf16Slice(s, start, count) {
    const text = hostUtf16Text(s);
    const from = Math.max(0, start | 0);
    const len = Math.max(0, count | 0);
    return hostIntern(text.slice(from, from + len));
}

// JS `indexOf` returns a UTF-16 code-unit index — the std contract.
function hostUtf16IndexOf(s, other) {
    return hostUtf16Text(s).indexOf(hostUtf16Text(other));
}

// 与 `Utf8Text::trim` 同一份 `White_Space` 集合，但按 code unit 前进：代理项
// 不在集合内，因此 lone surrogate 会被保留。
function hostUtf16Trim(s) {
    const text = hostUtf16Text(s);
    let start = 0;
    let end = text.length;
    while (start < end && UTF8_WHITE_SPACE.has(text.codePointAt(start))) start += 1;
    while (end > start && UTF8_WHITE_SPACE.has(text.codePointAt(end - 1))) end -= 1;
    return hostIntern(text.slice(start, end));
}

function hostUtf16ToLower(s) {
    return hostIntern(hostUtf16Text(s).toLowerCase());
}

function hostUtf16ToUpper(s) {
    return hostIntern(hostUtf16Text(s).toUpperCase());
}

function hostUtf16Replace(s, oldValue, newValue) {
    const text = hostUtf16Text(s);
    const needle = hostUtf16Text(oldValue);
    const replacement = hostUtf16Text(newValue);
    if (needle.length === 0) return hostIntern(text);
    return hostIntern(text.split(needle).join(replacement));
}

function hostUtf16StartsWith(s, prefix) {
    return hostUtf16Text(s).startsWith(hostUtf16Text(prefix)) ? 1 : 0;
}

function hostUtf16EndsWith(s, suffix) {
    return hostUtf16Text(s).endsWith(hostUtf16Text(suffix)) ? 1 : 0;
}

function hostUtf16Contains(s, other) {
    return hostUtf16Text(s).includes(hostUtf16Text(other)) ? 1 : 0;
}

function hostUtf16Equals(a, b) {
    return hostUtf16Text(a) === hostUtf16Text(b) ? 1 : 0;
}
"#;

/// 一个文本操作的 provider 条目（物理签名全部为 i32 宿主句柄载体）。
fn text_provider(field: &str, params: usize, implementation: &str, helpers: &str) -> WasmHostImportProviderFact {
    WasmHostImportProviderFact {
        module: "env".to_string(),
        field: field.to_string(),
        params: vec![WasmAbiCarrier::Handle; params],
        results: vec![WasmAbiCarrier::Handle],
        implementation: implementation.to_string(),
        helpers: helpers.to_string(),
    }
}

/// JS glue 边界提交的语言 std 文本 provider 全集。
///
/// `utf8_*` 家族此前由 wasm 降层无条件登记（语言行为内建于目标 lane）；
/// `utf16_*` 家族与编码转换此前由 JS glue 硬编码实现。现在统一由这里声明。
pub(crate) fn text_host_import_providers() -> Vec<WasmHostImportProviderFact> {
    let mut providers = vec![
        text_provider("utf8_concat", 2, "(a, b) => hostUtf8Concat(a, b)", TEXT_HELPERS),
        text_provider("utf8_length", 1, "(s) => hostUtf8Length(s)", ""),
        text_provider("utf8_trim", 1, "(s) => hostIntern(hostUtf8Trim(s))", ""),
        text_provider("utf8_to_lower", 1, "(s) => hostIntern(hostUtf8ToLower(s))", ""),
        text_provider("utf8_to_upper", 1, "(s) => hostIntern(hostUtf8ToUpper(s))", ""),
        text_provider("utf8_replace", 3, "(s, oldValue, newValue) => hostIntern(hostUtf8Replace(s, oldValue, newValue))", ""),
        text_provider("utf8_starts_with", 2, "(s, prefix) => hostUtf8StartsWith(s, prefix)", ""),
        text_provider("utf8_ends_with", 2, "(s, suffix) => hostUtf8EndsWith(s, suffix)", ""),
        text_provider("utf8_contains", 2, "(s, other) => hostUtf8Contains(s, other)", ""),
        text_provider("utf8_equals", 2, "(a, b) => hostUtf8Equals(a, b)", ""),
        text_provider("utf8_index_of", 2, "(s, other) => hostUtf8IndexOf(s, other)", ""),
        text_provider("utf8_slice", 3, "(s, start, count) => hostIntern(hostUtf8Slice(s, start, count))", ""),
        text_provider("utf8_to_utf16", 1, "(s) => hostIntern(hostPathFromRef(s))", ""),
        text_provider("utf16_to_utf8", 1, "(s) => hostIntern(hostPathFromRef(s))", ""),
        text_provider("utf16_concat", 2, "(a, b) => hostUtf16Concat(a, b)", ""),
        text_provider("utf16_length", 1, "(s) => hostUtf16Length(s)", ""),
        text_provider("utf16_trim", 1, "(s) => hostUtf16Trim(s)", ""),
        text_provider("utf16_to_lower", 1, "(s) => hostUtf16ToLower(s)", ""),
        text_provider("utf16_to_upper", 1, "(s) => hostUtf16ToUpper(s)", ""),
        text_provider("utf16_replace", 3, "(s, oldValue, newValue) => hostUtf16Replace(s, oldValue, newValue)", ""),
        text_provider("utf16_starts_with", 2, "(s, prefix) => hostUtf16StartsWith(s, prefix)", ""),
        text_provider("utf16_ends_with", 2, "(s, suffix) => hostUtf16EndsWith(s, suffix)", ""),
        text_provider("utf16_contains", 2, "(s, other) => hostUtf16Contains(s, other)", ""),
        text_provider("utf16_equals", 2, "(a, b) => hostUtf16Equals(a, b)", ""),
        text_provider("utf16_index_of", 2, "(s, other) => hostUtf16IndexOf(s, other)", ""),
        text_provider("utf16_slice", 3, "(s, start, count) => hostUtf16Slice(s, start, count)", ""),
    ];
    debug_assert!(providers.iter().filter(|provider| provider.field == "utf8_concat").count() == 1, "共享辅助 JS 只随一个条目注入");
    debug_assert!(providers.iter().filter(|provider| !provider.helpers.is_empty()).count() == 1, "共享辅助 JS 只注入一次");
    providers
}
