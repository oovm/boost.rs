# `interpreter-jit`

Language-agnostic JIT substrate: compile requests, conservative stack maps, deopt frame materialization, versioned scalar machine-code contract with pluggable format matchers

Optional JIT acceleration substrate for interpreter runtimes.

语言无关：只消费已验证执行格式的字节码视图（[`JitCompileRequest`]）。
格式适配器与通用调度分离：标量模式匹配经 [`ScalarProgramMatcher`] 由目标 VM 注入，
本 crate 不依赖任何具体语言、VM 或字节码格式 crate。

机器码合同见 [`machine_code`]：版本化的可解释 / 可降低中间字节，**不是** OS
可执行页上的原生指令；真实原生发射属于 `compiler-emitter` 的后续闭包。
保守 GC stack map 见 [`stack_map`]，去优化帧恢复合同见 [`deopt`]，
可失效编译假设见 [`assumption`]。
