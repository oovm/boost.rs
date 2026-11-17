//! `boost-collection` 的形状、索引与所有权验收。
//!
//! 覆盖：行主序生成与 flat 顺序、坐标越界、零 extent 与 rank 0、
//! 乘积溢出与长度不匹配拒绝、非 Copy 元素的初始化/移动/释放、
//! 二维行切片，以及编译期固定形状与 `array!` 元组展开。

use boost_collection::{array, Array2D, ArrayND, CollectionError};
use std::{
    cell::Cell,
    rc::Rc,
};

#[test]
fn row_major_generation_and_access() {
    let grid = ArrayND::new([2, 3], |[row, column]| (row * 3 + column) as i32).expect("shape fits");
    assert_eq!(grid.shape(), [2, 3]);
    assert_eq!(grid.len(), 6);
    assert_eq!(grid.as_slice(), &[0, 1, 2, 3, 4, 5]);
    assert_eq!(grid.get([1, 2]), Some(&5));
    assert_eq!(grid[[0, 1]], 1);
    assert_eq!(grid.get([2, 0]), None);
    assert_eq!(grid.get([1, 3]), None);
    assert_eq!(grid.into_vec(), vec![0, 1, 2, 3, 4, 5]);
}

#[test]
fn zero_extents_keep_their_shape() {
    let rowless = ArrayND::<u8, 2>::new([0, 5], |_| unreachable!("no element is generated")).expect("empty shape");
    let columnless = ArrayND::<u8, 2>::new([3, 0], |_| unreachable!("no element is generated")).expect("empty shape");
    assert!(rowless.is_empty() && columnless.is_empty());
    assert_eq!(rowless.shape(), [0, 5]);
    assert_eq!(columnless.shape(), [3, 0]);

    // 零 extent 必须先于非零轴的乘积检查，合法空数组不能因溢出被拒。
    let huge_empty = ArrayND::<u8, 2>::new([0, usize::MAX], |_| unreachable!("no element is generated"))
        .expect("zero extent wins over the product");
    assert_eq!(huge_empty.len(), 0);
}

#[test]
fn rank_zero_is_one_scalar_slot() {
    let scalar = ArrayND::new([], |[]| 7_u8).expect("rank zero");
    assert_eq!(scalar.rank(), 0);
    assert_eq!(scalar.len(), 1);
    assert_eq!(scalar.get([]), Some(&7));
    assert_eq!(scalar[[]], 7);
    assert_eq!(scalar.as_slice(), &[7]);
}

#[test]
fn shape_overflow_and_length_mismatch_are_rejected() {
    assert_eq!(ArrayND::<u8, 2>::new([usize::MAX, 2], |_| 0).unwrap_err(), CollectionError::ShapeOverflow);

    let error = ArrayND::from_vec(vec![1_u8, 2, 3], [2, 2]).unwrap_err();
    assert_eq!(error, CollectionError::ElementCountMismatch { shape: 4, elements: 3 });
}

#[test]
fn elements_are_writable_without_changing_the_shape() {
    let mut grid = ArrayND::filled([2, 2], 0_u8).expect("shape fits");
    grid[[0, 0]] = 9;
    *grid.get_mut([1, 1]).expect("in bounds") = 4;
    grid.as_mut_slice()[1] = 2;
    assert_eq!(grid.as_slice(), &[9, 2, 0, 4]);
    assert_eq!(grid.shape(), [2, 2]);
}

#[test]
fn row_slices_are_bounded() {
    let mut grid = ArrayND::from_vec(vec![1_u8, 2, 3, 4, 5, 6], [2, 3]).expect("shape matches");
    assert_eq!(grid.row(1), Some(&[4, 5, 6][..]));
    assert_eq!(grid.row(2), None);
    grid.row_mut(0).expect("in bounds").copy_from_slice(&[7, 8, 9]);
    assert_eq!(grid.as_slice(), &[7, 8, 9, 4, 5, 6]);
}

/// 记录 drop 次数的非 Copy 元素。
struct Token {
    value: u32,
    dropped: Rc<Cell<usize>>,
}

impl Token {
    fn new(value: u32, dropped: &Rc<Cell<usize>>) -> Self {
        Self { value, dropped: Rc::clone(dropped) }
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        self.dropped.set(self.dropped.get() + 1);
    }
}

#[test]
fn non_copy_elements_move_and_drop_exactly_once() {
    let dropped = Rc::new(Cell::new(0));
    let grid = ArrayND::new([2, 2], |[row, column]| Token::new((row * 2 + column) as u32, &dropped)).expect("shape fits");
    assert_eq!(grid[[1, 0]].value, 2);
    assert_eq!(dropped.get(), 0);

    // 部分消费：被取出的元素和被丢弃的迭代器各释放一次。
    let mut iter = grid.into_iter();
    let first = iter.next().expect("non-empty");
    assert_eq!(first.value, 0);
    drop(first);
    assert_eq!(dropped.get(), 1);
    drop(iter);
    assert_eq!(dropped.get(), 4);
}

#[test]
fn compile_time_shape_and_tuple_macro() {
    type Grid = array!(f32, (3, 3));
    let mut grid = Grid::filled(0.0);
    assert_eq!(grid.shape(), [3, 3]);
    assert_eq!(grid.len(), 9);
    grid[[1, 2]] = 5.0;
    assert_eq!(grid[[1, 2]], 5.0);
    assert_eq!(grid.row(1), Some(&[0.0, 0.0, 5.0]));
    assert_eq!(grid.row(3), None);

    type Inline = array!(u8, 2, 4);
    assert_eq!(Inline::new(|[row, column]| (row * 4 + column) as u8).shape(), [2, 4]);

    let source = [[1_u8, 2], [3, 4]];
    let converted: Array2D<u8, 2, 2> = Array2D::from(source);
    assert_eq!(converted.iter().copied().collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    assert_eq!(converted.into_inner(), source);
}

#[test]
fn compile_time_shape_keeps_zero_extents() {
    let empty = Array2D::<u8, 0, 4>::new(|_| unreachable!("no element is generated"));
    assert!(empty.is_empty());
    assert_eq!(empty.shape(), [0, 4]);
    assert_eq!(empty.get([0, 0]), None);
}
