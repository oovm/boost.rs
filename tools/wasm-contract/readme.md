# wasm-contract

隔离的 Wasm 互操作验证工具。它不是 Boost workspace 的成员（自带空 `[workspace]` 表），
其 `wasmtime` 依赖不进入 Boost 的依赖图与 `Cargo.lock`，只在手动运行时解析。

## 作用

把 `hardware-wasm` 构建出的 `wasm32-unknown-unknown` 产物交给 `wasmtime` 实例化，
按宿主视角调用导出的 C ABI，验收应用契约（矩阵乘、诊断位掩码、L0 读回、L2 提交、
外部 buffer 绑定）。

## 运行

```bash
rustup target add wasm32-unknown-unknown
cargo build -p hardware-wasm --target wasm32-unknown-unknown --release

# 在本仓根执行
cargo test --manifest-path tools/wasm-contract/Cargo.toml
```

默认读取 `target/wasm32-unknown-unknown/release/hardware_wasm.wasm`。产物在别处时用
`HARDWARE_WASM_ARTIFACT` 指向它：

```bash
$env:HARDWARE_WASM_ARTIFACT = "D:\artifacts\hardware_wasm.wasm"
cargo test --manifest-path tools/wasm-contract/Cargo.toml
```

产物缺失时脚本不执行任何断言，而是直接向终端打印 `SKIP:` 与原因并以 `0` 退出，
避免把「没构建」误判成通过。
