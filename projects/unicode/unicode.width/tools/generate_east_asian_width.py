#!/usr/bin/env python3
"""生成 `src/tables/east_asian_width.rs`。

只读取 `unicode.types` 已入库的 UCD 输入文件，不联网、不依赖第三方库，重复运行产出
逐字节相同的文件。
用法：`python tools/generate_east_asian_width.py`
"""

import hashlib
import os

GENERATOR_VERSION = 1
UNICODE_VERSION = "18.0.0"
INPUT_RELATIVE = "../unicode.types/data/ucd/%s/EastAsianWidth.txt" % UNICODE_VERSION
MAX_CODE_POINT = 0x10FFFF

CRATE_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
INPUT_PATH = os.path.normpath(os.path.join(CRATE_ROOT, INPUT_RELATIVE))
OUTPUT_PATH = os.path.join(CRATE_ROOT, "src", "tables", "east_asian_width.rs")

# UAX #11 的六个 East_Asian_Width 取值，顺序固定为 A/F/H/N/Na/W。
VALUES = ["A", "F", "H", "N", "Na", "W"]

VARIANTS = {
    "N": "Neutral",
    "A": "Ambiguous",
    "H": "Halfwidth",
    "F": "Fullwidth",
    "W": "Wide",
    "Na": "Narrow",
}

# 输入文件头声明的 `@missing` 约定：未列出的码点默认 `N`，但下列范围里未指派的码点
# 默认 `W`。只用于填补文件未列出的码点，已列出的取值不受影响。
WIDE_DEFAULT_RANGES = [
    (0x3400, 0x4DBF),   # CJK Unified Ideographs Extension A
    (0x4E00, 0x9FFF),   # CJK Unified Ideographs
    (0xF900, 0xFAFF),   # CJK Compatibility Ideographs
    (0x20000, 0x2FFFD), # Plane 2
    (0x30000, 0x3FFFD), # Plane 3
]


def read_input():
    with open(INPUT_PATH, "rb") as handle:
        return handle.read()


def default_value(code_point):
    for start, end in WIDE_DEFAULT_RANGES:
        if start <= code_point <= end:
            return "W"
    return "N"


def parse_intervals(raw):
    """解析 `EastAsianWidth.txt`，返回按码点升序、互不重叠的显式区间。"""
    intervals = []
    for line in raw.decode("utf-8").splitlines():
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        fields = [field.strip() for field in line.split(";")]
        if len(fields) != 2:
            raise ValueError("unexpected EastAsianWidth line: %r" % line)
        code_range, value = fields
        if value not in VALUES:
            raise ValueError("unknown East_Asian_Width %r on %r" % (value, line))
        if ".." in code_range:
            start_text, end_text = code_range.split("..")
            start, end = int(start_text, 16), int(end_text, 16)
        else:
            start = end = int(code_range, 16)
        if end < start:
            raise ValueError("inverted range: %r" % line)
        intervals.append((start, end, value))
    intervals.sort()
    for index in range(1, len(intervals)):
        if intervals[index][0] <= intervals[index - 1][1]:
            raise ValueError("overlapping intervals at U+%04X" % intervals[index][0])
    return intervals


def build_runs(intervals):
    """把显式区间与 `@missing` 默认值合并成覆盖完整码点空间的 run-length 表。"""
    runs = []
    cursor = 0

    def push(start, end, value):
        if start > end:
            return
        if runs and runs[-1][2] == value and runs[-1][1] + 1 == start:
            runs[-1] = (runs[-1][0], end, value)
        else:
            runs.append((start, end, value))

    for start, end, value in intervals:
        if start > cursor:
            push_gap(runs, cursor, start - 1)
        push(start, end, value)
        cursor = end + 1
    if cursor <= MAX_CODE_POINT:
        push_gap(runs, cursor, MAX_CODE_POINT)
    if not runs or runs[0][0] != 0 or runs[-1][1] != MAX_CODE_POINT:
        raise ValueError("runs do not cover the code point space")
    for index in range(1, len(runs)):
        if runs[index][0] != runs[index - 1][1] + 1:
            raise ValueError("gap or overlap before U+%04X" % runs[index][0])
        if runs[index][2] == runs[index - 1][2]:
            raise ValueError("unmerged run before U+%04X" % runs[index][0])
    return runs


def push_gap(runs, start, end):
    """按 `@missing` 约定填充一段未列出的码点，必要时在 `W` 默认区段内切分。"""
    code_point = start
    while code_point <= end:
        value = default_value(code_point)
        segment_end = code_point
        while segment_end + 1 <= end and default_value(segment_end + 1) == value:
            segment_end += 1
        if runs and runs[-1][2] == value and runs[-1][1] + 1 == code_point:
            runs[-1] = (runs[-1][0], segment_end, value)
        else:
            runs.append((code_point, segment_end, value))
        code_point = segment_end + 1


def render(runs, sha256):
    lines = []
    lines.append("// 本文件由 `tools/generate_east_asian_width.py` 生成，请勿手工修改。")
    lines.append("// 输入文件：`%s`" % INPUT_RELATIVE)
    lines.append("// 输入 SHA-256：%s" % sha256)
    lines.append("// 生成器版本：%d" % GENERATOR_VERSION)
    lines.append("// Unicode 版本：%s" % UNICODE_VERSION)
    lines.append("")
    lines.append("use crate::east_asian_width::EastAsianWidth;")
    lines.append("")
    lines.append("/// `U+0000..=U+10FFFF` 的 East_Asian_Width 区间表。")
    lines.append("///")
    lines.append("/// 按码点升序排列，区间互不重叠也无断档：首区间始于 `U+0000`，末区间止于 `U+10FFFF`。")
    lines.append("/// 文件未列出的码点按 UCD 的 `@missing` 约定取值，CJK 相关区段里未指派的码点取 `W`。")
    lines.append("pub const EAST_ASIAN_WIDTH: &[(u32, u32, EastAsianWidth)] = &[")
    for start, end, value in runs:
        lines.append("    (0x%04X, 0x%04X, EastAsianWidth::%s)," % (start, end, VARIANTS[value]))
    lines.append("];")
    return "\n".join(lines) + "\n"


def main():
    raw = read_input()
    sha256 = hashlib.sha256(raw).hexdigest()
    runs = build_runs(parse_intervals(raw))
    text = render(runs, sha256)
    os.makedirs(os.path.dirname(OUTPUT_PATH), exist_ok=True)
    with open(OUTPUT_PATH, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)
    print("wrote %s (%d runs)" % (os.path.relpath(OUTPUT_PATH, CRATE_ROOT), len(runs)))


if __name__ == "__main__":
    main()
