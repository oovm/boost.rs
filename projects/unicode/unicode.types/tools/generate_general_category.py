#!/usr/bin/env python3
"""生成 `src/tables/general_category.rs`。

只读取仓内入库的 UCD 输入文件，不联网、不依赖第三方库，重复运行产出逐字节相同的文件。
用法：`python tools/generate_general_category.py`
"""

import hashlib
import os

GENERATOR_VERSION = 1
UNICODE_VERSION = "16.0.0"
INPUT_NAME = "UnicodeData.txt"
MAX_CODE_POINT = 0x10FFFF

CRATE_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
INPUT_PATH = os.path.join(CRATE_ROOT, "data", "ucd", UNICODE_VERSION, INPUT_NAME)
OUTPUT_PATH = os.path.join(CRATE_ROOT, "src", "tables", "general_category.rs")

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


def read_input():
    with open(INPUT_PATH, "rb") as handle:
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
