# `boost-parallel`

确定性作用域并行：借用切片、按输入顺序合并、支持协作式取消。

- `for_each`：并行遍历元素，副作用顺序不作保证。
- `map`：并行映射，输出按输入下标排序，与串行结果同序。
- `for_each_range`：按下标区间动态分块，块边界检查 `Cancel`。
- `threads` / `Cancel`：复用 `boost-concurrent` 的线程数策略与取消令牌。

只使用 `std::thread::scope` 借用调用方数据，不要求 `'static`，不使用 `unsafe`；
不做工作窃取。提前退出只在分块边界生效，已开始的块会完整执行。
