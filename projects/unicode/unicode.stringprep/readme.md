# `unicode-stringprep`

字符串准备（StringPrep）家族：RFC 4013 SASLprep 口令规范化，登记为 unicode.stringprep

字符串准备（StringPrep）家族：RFC 4013 SASLprep。

本 crate 拥有 SASL 口令预处理的唯一实现：表 B.1/C.1.2 映射、NFKC 规范化、
RFC 3454 附录 C 禁止集合与 §6 双向检查，顺序固定为映射 → 规范化 → 禁止 → 双向。
NFKC 由 `unicode-normalization` 提供（内化未完成，不能记为已签收）；
表内容由 `tools/generate_saslprep_tables.py` 从 CPython `stringprep` 生成。
协议侧（SCRAM 等）只在这里取结果，不再各自维护口令规范化副本。
