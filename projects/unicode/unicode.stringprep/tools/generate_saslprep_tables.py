#!/usr/bin/env python3
"""生成 `src/tables.rs`：RFC 3454 附录 B/C 与 D.1/D.2 的码点区间表。

映射与禁止表取自 CPython 标准库 `stringprep`（RFC 3454 表的独立权威编码，
冻结于 Unicode 3.2）；D.1/D.2 取自 `unicodedata` 的现代双向类
（R/AL → RandALCat，L → LCat），与 mongod 使用的 ICU 行为同口径。
不联网、不依赖第三方库，重复运行产出逐字节相同的文件。
用法：`python tools/generate_saslprep_tables.py`
"""

import stringprep
import unicodedata

GENERATOR_VERSION = 1
MAX_CODE_POINT = 0x10FFFF
OUTPUT_PATH = "src/tables.rs"


def runs(character_predicate):
    """把逐码点谓词合并成 `(start, end)` 升序区间。"""

    def predicate(code):
        return character_predicate(chr(code))

    merged = []
    start = None
    for code in range(MAX_CODE_POINT + 1):
        if predicate(code):
            if start is None:
                start = code
        elif start is not None:
            merged.append((start, code - 1))
            start = None
    if start is not None:
        merged.append((start, MAX_CODE_POINT))
    return merged


def in_any(*predicates):
    def test(character):
        return any(predicate(character) for predicate in predicates)

    return test


def main():
    tables = {
        "MAPPED_TO_NOTHING": runs(stringprep.in_table_b1),
        "MAPPED_TO_SPACE": runs(stringprep.in_table_c12),
        # RFC 4013 §2.3：禁止集合为 C.2.1、C.2.2、C.3、C.4、C.5、C.6、C.7、C.8、C.9。
        # C.1.1（ASCII 空格）与 C.1.2（非 ASCII 空格）是映射目标，不在禁止集合内。
        "PROHIBITED": runs(
            in_any(
                stringprep.in_table_c21_c22,
                stringprep.in_table_c3,
                stringprep.in_table_c4,
                stringprep.in_table_c5,
                stringprep.in_table_c6,
                stringprep.in_table_c7,
                stringprep.in_table_c8,
                stringprep.in_table_c9,
            )
        ),
        "RAND_ALCAT": runs(lambda c: unicodedata.bidirectional(c) in ("R", "AL")),
        "L_CAT": runs(lambda c: unicodedata.bidirectional(c) == "L"),
    }

    lines = []
    lines.append("// 本文件由 `tools/generate_saslprep_tables.py` 生成，请勿手工修改。")
    lines.append("// 输入：CPython `stringprep`（RFC 3454 附录 B.1/C 表）与 `unicodedata` 现代双向类（D.1/D.2）。")
    lines.append("// 生成器版本：%d" % GENERATOR_VERSION)
    lines.append("")

    descriptions = {
        "MAPPED_TO_NOTHING": "RFC 3454 表 B.1（commonly mapped to nothing）的码点区间。",
        "MAPPED_TO_SPACE": "RFC 3454 表 C.1.2（non-ASCII space）的码点区间。",
        "PROHIBITED": "RFC 4013 §2.3 禁止集合：C.2.1、C.2.2、C.3、C.4、C.5、C.6、C.7、C.8、C.9。",
        "RAND_ALCAT": "RandALCat（双向类 R/AL，现代 Unicode）的码点区间。",
        "L_CAT": "LCat（双向类 L，现代 Unicode）的码点区间。",
    }
    for name, ranges in tables.items():
        lines.append("/// %s" % descriptions[name])
        lines.append("///")
        lines.append("/// 按码点升序排列，区间互不重叠。")
        lines.append("pub const %s: &[(u32, u32)] = &[" % name)
        for start, end in ranges:
            lines.append("    (0x%04X, 0x%04X)," % (start, end))
        lines.append("];")
        lines.append("")

    with open(OUTPUT_PATH, "w", encoding="utf-8", newline="\n") as handle:
        handle.write("\n".join(lines).rstrip() + "\n")
    summary = ", ".join("%s=%d" % (name, len(ranges)) for name, ranges in tables.items())
    print("wrote %s (%s)" % (OUTPUT_PATH, summary))


if __name__ == "__main__":
    main()
