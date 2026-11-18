#!/usr/bin/env python3
"""生成 `src/tables/general_category.rs` 与 `src/tables/derived_core.rs`。

只读取仓内入库的 UCD 输入文件，不联网、不依赖第三方库，重复运行产出逐字节相同的文件。
用法：`python tools/generate_general_category.py`
"""

import hashlib
import os

GENERATOR_VERSION = 2
UNICODE_VERSION = "18.0.0"
INPUT_NAME = "UnicodeData.txt"
DERIVED_INPUT_NAME = "DerivedCoreProperties.txt"
MAX_CODE_POINT = 0x10FFFF

CRATE_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
UCD_DIR = os.path.join(CRATE_ROOT, "data", "ucd", UNICODE_VERSION)
INPUT_PATH = os.path.join(UCD_DIR, INPUT_NAME)
DERIVED_INPUT_PATH = os.path.join(UCD_DIR, DERIVED_INPUT_NAME)
OUTPUT_PATH = os.path.join(CRATE_ROOT, "src", "tables", "general_category.rs")
DERIVED_OUTPUT_PATH = os.path.join(CRATE_ROOT, "src", "tables", "derived_core.rs")

# Unicode 标准的 30 个 General_Category 值，顺序固定为 UCD 的官方排列。
GENERAL_CATEGORIES = [
    "Lu", "Ll", "Lt", "Lm", "Lo",
    "Mn", "Mc", "Me",
    "Nd", "Nl", "No",
    "Pc", "Pd", "Ps", "Pe", "Pi", "Pf", "Po",
    "Sm", "Sc", "Sk", "So",
    "Zs", "Zl", "Zp",
    "Cc", "Cf", "Cs", "Co", "Cn",
]

# 从 `DerivedCoreProperties.txt` 提取的派生属性，映射到生成的常量名。
DERIVED_PROPERTIES = [
    ("Grapheme_Extend", "GRAPHEME_EXTEND"),
    ("Default_Ignorable_Code_Point", "DEFAULT_IGNORABLE"),
]


def read_input(path):
    with open(path, "rb") as handle:
        return handle.read()


def parse_intervals(raw):
    """把 UnicodeData.txt 解析为互不重叠的码点区间，`First>`/`Last>` 成对展开。"""
    intervals = []
    pending = None
    for line in raw.decode("utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        fields = line.split(";")
        if len(fields) < 3:
            raise ValueError("unexpected UnicodeData line: %r" % line)
        code_point = int(fields[0], 16)
        name = fields[1]
        category = fields[2]
        if category not in GENERAL_CATEGORIES:
            raise ValueError("unknown General_Category %r on %r" % (category, line))
        if name.endswith(", First>"):
            pending = (code_point, category, name[:-len(", First>")])
            continue
        if name.endswith(", Last>"):
            if pending is None:
                raise ValueError("Last> without First>: %r" % line)
            start, start_category, start_name = pending
            if start_category != category or start_name != name[:-len(", Last>")]:
                raise ValueError("mismatched range: %r" % line)
            if code_point < start:
                raise ValueError("inverted range: %r" % line)
            intervals.append((start, code_point, category))
            pending = None
            continue
        if pending is not None:
            raise ValueError("unterminated range before %r" % line)
        intervals.append((code_point, code_point, category))
    if pending is not None:
        raise ValueError("unterminated range at end of file")
    intervals.sort()
    return intervals


def build_runs(intervals):
    """把区间合并成 run-length 表，未分配的码点填 `Cn`。"""
    runs = []
    cursor = 0

    def push(start, end, category):
        if runs and runs[-1][2] == category and runs[-1][1] + 1 == start:
            runs[-1] = (runs[-1][0], end, category)
        else:
            runs.append((start, end, category))

    for start, end, category in intervals:
        if start < cursor:
            raise ValueError("overlapping intervals at U+%04X" % start)
        if start > cursor:
            push(cursor, start - 1, "Cn")
        push(start, end, category)
        cursor = end + 1
    if cursor <= MAX_CODE_POINT:
        push(cursor, MAX_CODE_POINT, "Cn")
    if runs[0][0] != 0 or runs[-1][1] != MAX_CODE_POINT:
        raise ValueError("runs do not cover the code point space")
    return runs


def parse_property(raw, property_name):
    """提取 `DerivedCoreProperties.txt` 里单个属性的显式区间，合并相邻并校验不重叠。"""
    intervals = []
    for line in raw.decode("utf-8").splitlines():
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        fields = [field.strip() for field in line.split(";")]
        if len(fields) < 2:
            raise ValueError("unexpected DerivedCoreProperties line: %r" % line)
        code_range, name = fields[0], fields[1]
        if name != property_name:
            continue
        if ".." in code_range:
            start_text, end_text = code_range.split("..")
            start, end = int(start_text, 16), int(end_text, 16)
        else:
            start = end = int(code_range, 16)
        if end < start:
            raise ValueError("inverted range: %r" % line)
        intervals.append((start, end))
    intervals.sort()
    merged = []
    for start, end in intervals:
        if merged and start <= merged[-1][1]:
            raise ValueError("overlapping intervals at U+%04X" % start)
        if merged and merged[-1][1] + 1 == start:
            merged[-1] = (merged[-1][0], end)
        else:
            merged.append((start, end))
    if not merged:
        raise ValueError("no intervals for %r" % property_name)
    return merged


def render(runs, sha256):
    lines = []
    lines.append("// 本文件由 `tools/generate_general_category.py` 生成，请勿手工修改。")
    lines.append("// 输入文件：`data/ucd/%s/%s`" % (UNICODE_VERSION, INPUT_NAME))
    lines.append("// 输入 SHA-256：%s" % sha256)
    lines.append("// 生成器版本：%d" % GENERATOR_VERSION)
    lines.append("// Unicode 版本：%s" % UNICODE_VERSION)
    lines.append("")
    lines.append("use crate::general_category::GeneralCategory;")
    lines.append("")
    lines.append("/// `U+0000..=U+10FFFF` 的 General_Category 区间表。")
    lines.append("///")
    lines.append("/// 按码点升序排列，区间互不重叠也无断档：首区间始于 `U+0000`，末区间止于 `U+10FFFF`。")
    lines.append("pub const GENERAL_CATEGORY: &[(u32, u32, GeneralCategory)] = &[")
    for start, end, category in runs:
        lines.append("    (0x%04X, 0x%04X, GeneralCategory::%s)," % (start, end, category))
    lines.append("];")
    return "\n".join(lines) + "\n"


def render_derived(properties, sha256):
    lines = []
    lines.append("// 本文件由 `tools/generate_general_category.py` 生成，请勿手工修改。")
    lines.append("// 输入文件：`data/ucd/%s/%s`" % (UNICODE_VERSION, DERIVED_INPUT_NAME))
    lines.append("// 输入 SHA-256：%s" % sha256)
    lines.append("// 生成器版本：%d" % GENERATOR_VERSION)
    lines.append("// Unicode 版本：%s" % UNICODE_VERSION)
    for property_name, constant in DERIVED_PROPERTIES:
        lines.append("")
        lines.append("/// 派生属性 `%s` 的码点区间表。" % property_name)
        lines.append("///")
        lines.append("/// 按码点升序排列，区间互不重叠，只覆盖该属性为真的码点。")
        lines.append("pub const %s: &[(u32, u32)] = &[" % constant)
        for start, end in properties[property_name]:
            lines.append("    (0x%04X, 0x%04X)," % (start, end))
        lines.append("];")
    return "\n".join(lines) + "\n"


def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)


def main():
    raw = read_input(INPUT_PATH)
    sha256 = hashlib.sha256(raw).hexdigest()
    runs = build_runs(parse_intervals(raw))
    write(OUTPUT_PATH, render(runs, sha256))
    print("wrote %s (%d runs)" % (os.path.relpath(OUTPUT_PATH, CRATE_ROOT), len(runs)))

    derived_raw = read_input(DERIVED_INPUT_PATH)
    derived_sha256 = hashlib.sha256(derived_raw).hexdigest()
    properties = {
        name: parse_property(derived_raw, name) for name, _ in DERIVED_PROPERTIES
    }
    write(DERIVED_OUTPUT_PATH, render_derived(properties, derived_sha256))
    summary = ", ".join("%s=%d" % (name, len(properties[name])) for name, _ in DERIVED_PROPERTIES)
    print("wrote %s (%s)" % (os.path.relpath(DERIVED_OUTPUT_PATH, CRATE_ROOT), summary))


if __name__ == "__main__":
    main()
