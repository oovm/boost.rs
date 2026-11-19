# `document-harness`

document.* 下游验收共享夹具：报告断言、容器 fixture 构造与宿主字体包解析

`document.*` 下游验收共享夹具（`document.harness`）。

Boost 的 document 家族只保留最小冒烟测试；功能级验收在下游产品仓
（panduck / pandark / VuTeX）执行。本 crate 提供这些下游重复需要的验收件，
避免每个下游各抄一份：

- [`report`]：`panduck.report/v0` 的结构断言（损失镜像、状态、输出登记）。
- [`fixtures`]：内存构造 OOXML 容器 fixture（stored zip / docx），不落二进制资产。
- [`font_pack`]：宿主字体包解析（`VUTEX_FONT_DIR` 或仓旁 `VuTeX` 检出）。
