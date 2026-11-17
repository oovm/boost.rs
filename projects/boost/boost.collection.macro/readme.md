# `boost-collection-macro`

`boost-collection` 固定形状数组的过程宏。稳定 Rust 不能把元组当作 const 泛型参数，`Array2D<f32, (3, 3)>` 不是合法类型；`array!` 把元组拆成对应数量的 const 参数，展开结果仍是普通类型别名。

```rust
use boost_collection::array;

type Grid = array!(f32, (3, 3));
type Cube = array!(f32, (2, 3, 4));
// 等价：array!(f32, 3, 3) 与 array!(f32, 2, 3, 4)
```

| 宏 | 展开目标 | 适用 |
|----|----------|------|
| `array!` | `::boost_collection::Array2D` / `Array3D` | 直接依赖 `boost-collection` |
| `facade_array!` | `::boost::collection::Array2D` / `Array3D` | 只依赖 `boost` 门面 |

`boost` 门面把 `facade_array!` 重导出为 `array!`，因此门面消费者写 `boost::collection::array!(f32, (3, 3))`。两个 extent 展开二维，三个展开三维；轴长可以是任意 const 表达式，不限于字面量。
