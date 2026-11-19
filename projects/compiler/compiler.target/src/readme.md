# `compiler-target`

Language-neutral target resolution, artifact planning and backend registration contracts for compilers

语言无关的目标解析、产物规划与后端注册合同。

# 分层

- [`abstractions`]：目标、产物种类与二进制形态的中立描述。
- [`target_profile`] / [`target_resolution`]：目标画像、宿主探测与解析结果。
- [`backends`]：后端描述、注册表与目标编码 trait；具体编码由各目标实现。
- [`planning`] / [`lanes`] / [`packaging`] / [`selection`]：分区计划、降低通道、
  产物集合与后端选择。
- 语言语义、函数体与目标私有 LIR 不在本 crate；它们由语言前端与目标发射器拥有。
