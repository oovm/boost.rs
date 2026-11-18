# `science-gc`

`science-gc` 是 Science CAS runtime 的基础内存与生命周期层。它提供 segmented non-moving heap、allocation header、scratch
frame、root registry、pin 与 `GcMode` 作用域 guard。

## 职责

- `ArenaHeap` / `GcHeap`：object / numeric / scratch 分区与 segment 元数据
- `AllocationHeader`：limb / object block 前缀，供 tracing 与 reclaim
- Root registry 与泛化 `Trace` 合同（不懂数学语义）
- Scratch `mark` / `rewind`（不参与普通 tracing）
- `GcMode::{Auto, Deferred, Disabled}` 与 `GcSuspendGuard` / `GcDeferGuard` / `GcPinGuard`

## 非职责

不实现 BigInt 算术、IR 重写、M-Graph claim、E-Graph、solver 调度或 TS/N-API。  
不依赖 `science-numeric` / `science-ir` / `science-cas` / SXO。

## 依赖

```text
science-types → science-gc → science-numeric → …
```

## 验证

```sh
cargo test -p science-gc --test main
cargo doc -p science-gc --no-deps
```
