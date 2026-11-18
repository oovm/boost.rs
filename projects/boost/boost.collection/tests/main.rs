//! `boost-collection` 的形状、索引与所有权验收。
//!
//! 覆盖：行主序生成与 flat 顺序、坐标越界、零 extent 与 rank 0、
//! 乘积溢出与长度不匹配拒绝、非 Copy 元素的初始化/移动/释放、
//! 二维行切片，以及编译期固定形状与 `array!` 元组展开。

use boost_collection::{Array2D, CollectionError, DenseArrayND, InlineArray2D, InlineArray3D, array};
use std::{cell::Cell, rc::Rc};

#[test]
fn row_major_generation_and_access() {
    let grid = DenseArrayND::new([2, 3], |[row, column]| (row * 3 + column) as i32).expect("shape fits");
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
    let rowless = DenseArrayND::<u8, 2>::new([0, 5], |_| unreachable!("no element is generated")).expect("empty shape");
    let columnless = DenseArrayND::<u8, 2>::new([3, 0], |_| unreachable!("no element is generated")).expect("empty shape");
    assert!(rowless.is_empty() && columnless.is_empty());
    assert_eq!(rowless.shape(), [0, 5]);
    assert_eq!(columnless.shape(), [3, 0]);

    // 零 extent 必须先于非零轴的乘积检查，合法空数组不能因溢出被拒。
    let huge_empty =
        DenseArrayND::<u8, 2>::new([0, usize::MAX], |_| unreachable!("no element is generated")).expect("zero extent wins over the product");
    assert_eq!(huge_empty.len(), 0);
}

#[test]
fn rank_zero_is_one_scalar_slot() {
    let scalar = DenseArrayND::new([], |[]| 7_u8).expect("rank zero");
    assert_eq!(scalar.rank(), 0);
    assert_eq!(scalar.len(), 1);
    assert_eq!(scalar.get([]), Some(&7));
    assert_eq!(scalar[[]], 7);
    assert_eq!(scalar.as_slice(), &[7]);
}

#[test]
fn shape_overflow_and_length_mismatch_are_rejected() {
    assert_eq!(DenseArrayND::<u8, 2>::new([usize::MAX, 2], |_| 0).unwrap_err(), CollectionError::ShapeOverflow);

    let error = DenseArrayND::from_vec(vec![1_u8, 2, 3], [2, 2]).unwrap_err();
    assert_eq!(error, CollectionError::ElementCountMismatch { shape: 4, elements: 3 });
}

#[test]
fn elements_are_writable_without_changing_the_shape() {
    let mut grid = DenseArrayND::filled([2, 2], 0_u8).expect("shape fits");
    grid[[0, 0]] = 9;
    *grid.get_mut([1, 1]).expect("in bounds") = 4;
    grid.as_mut_slice()[1] = 2;
    assert_eq!(grid.as_slice(), &[9, 2, 0, 4]);
    assert_eq!(grid.shape(), [2, 2]);
}

#[test]
fn row_slices_are_bounded() {
    let mut grid = DenseArrayND::from_vec(vec![1_u8, 2, 3, 4, 5, 6], [2, 3]).expect("shape matches");
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
    let grid = DenseArrayND::new([2, 2], |[row, column]| Token::new((row * 2 + column) as u32, &dropped)).expect("shape fits");
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
    let converted: InlineArray2D<u8, 2, 2> = InlineArray2D::from(source);
    assert_eq!(converted.iter().copied().collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    assert_eq!(converted.into_inner(), source);
}

#[test]
fn compile_time_shape_keeps_zero_extents() {
    let empty = InlineArray2D::<u8, 0, 4>::new(|_| unreachable!("no element is generated"));
    assert!(empty.is_empty());
    assert_eq!(empty.shape(), [0, 4]);
    assert_eq!(empty.get([0, 0]), None);
}

#[test]
fn compile_time_three_dimensional_shape() {
    type Cube = array!(u8, (2, 3, 4));
    let mut cube = Cube::new(|[first, second, third]| (first * 12 + second * 4 + third) as u8);
    assert_eq!(cube.shape(), [2, 3, 4]);
    assert_eq!(cube.rank(), 3);
    assert_eq!(cube.len(), 24);
    assert_eq!(cube[[1, 2, 3]], 23);
    assert_eq!(cube.get([2, 0, 0]), None);
    assert_eq!(cube.iter().copied().collect::<Vec<_>>(), (0..24).collect::<Vec<u8>>());

    cube[[0, 1, 2]] = 7;
    *cube.get_mut([0, 1, 3]).expect("in bounds") = 8;
    assert_eq!(cube.plane(0).expect("first plane")[1], [4, 5, 7, 8]);
    assert_eq!(cube.plane(2), None);

    type Flat = array!(u8, 1, 2, 3);
    assert_eq!(Flat::filled(0).len(), 6);
    assert_eq!(InlineArray3D::<u8, 1, 0, 3>::new(|_| unreachable!("no element is generated")).shape(), [1, 0, 3]);

    let source = [[[1_u8, 2], [3, 4]], [[5, 6], [7, 8]]];
    let converted: InlineArray3D<u8, 2, 2, 2> = InlineArray3D::from(source);
    assert_eq!(converted.into_inner(), source);
}

#[test]
fn view_exposes_roi_and_transpose_mappings() {
    let grid = DenseArrayND::new([2, 3], |[row, column]| (row * 3 + column) as i32).expect("shape fits");
    let view = grid.view();
    assert_eq!(view.shape(), [2, 3]);
    assert!(view.is_contiguous());
    assert_eq!(view.as_slice(), Some(&[0, 1, 2, 3, 4, 5][..]));

    let transposed = view.transpose();
    assert_eq!(transposed.shape(), [3, 2]);
    assert!(!transposed.is_contiguous());
    assert_eq!(transposed.as_slice(), None);
    assert_eq!(transposed.get([0, 1]), Some(&3));
    assert_eq!(transposed.get([2, 0]), Some(&2));
    assert_eq!(transposed.get([3, 0]), None);
    assert_eq!(transposed.iter().copied().collect::<Vec<_>>(), vec![0, 3, 1, 4, 2, 5]);

    let roi = grid.view_rect(0..2, 1..3).expect("in-bounds rect");
    assert_eq!(roi.shape(), [2, 2]);
    assert_eq!(roi.get([0, 0]), Some(&1));
    assert_eq!(roi.get([1, 1]), Some(&5));
    assert!(!roi.is_contiguous());
    assert_eq!(roi.iter().copied().collect::<Vec<_>>(), vec![1, 2, 4, 5]);
    assert!(grid.view_rect(0..3, 0..1).is_none());
    assert!(grid.view_rect(std::ops::Range { start: 1, end: 0 }, 0..1).is_none());

    // 转置视图上的 ROI 仍映射回原坐标。
    let corner = transposed.rect(1..3, 0..2).expect("in-bounds rect");
    assert_eq!(corner.shape(), [2, 2]);
    assert_eq!(corner.get([0, 0]), Some(&1));
    assert_eq!(corner.get([1, 1]), Some(&5));

    let zero = DenseArrayND::<u8, 2>::new([0, 4], |_| unreachable!("no element is generated")).expect("empty shape");
    let empty = zero.view();
    assert!(empty.is_contiguous());
    assert_eq!(empty.as_slice(), Some(&[][..]));
    assert_eq!(empty.iter().count(), 0);
}

#[test]
fn mutable_view_writes_through_roi_and_transpose() {
    let mut grid = DenseArrayND::new([2, 3], |[row, column]| (row * 3 + column) as i32).expect("shape fits");
    {
        let mut view = grid.view_mut();
        assert!(view.as_slice().is_some());
        view.as_mut_slice().expect("the full view is contiguous")[0] = 5;

        let mut corner = view.rect(0..2, 1..3).expect("in-bounds rect");
        assert!(corner.as_mut_slice().is_none(), "a column ROI is not contiguous");
        *corner.get_mut([1, 0]).expect("in bounds") = 20;

        let mut transposed = corner.transpose();
        *transposed.get_mut([1, 0]).expect("in bounds") = 30;
        assert_eq!(transposed.get([2, 0]), None);
    }
    assert_eq!(grid[[0, 0]], 5);
    assert_eq!(grid[[1, 1]], 20);
    assert_eq!(grid[[0, 2]], 30);
}

#[test]
fn reshape_reuses_the_buffer_and_keeps_row_major_order() {
    let grid = DenseArrayND::new([2, 3], |[row, column]| (row * 3 + column) as i32).expect("shape fits");
    let transposed = grid.clone().reshaped([3, 2]).expect("same element count");
    assert_eq!(transposed.shape(), [3, 2]);
    assert_eq!(transposed.as_slice(), &[0, 1, 2, 3, 4, 5]);
    assert_eq!(transposed[[1, 1]], 3);

    let flat = grid.clone().reshaped([6]).expect("rank may change");
    assert_eq!(flat.shape(), [6]);
    assert_eq!(flat[[4]], 4);

    let scalar = DenseArrayND::from_vec(vec![7_i32], []).expect("one scalar").reshaped([1, 1]).expect("same element count");
    assert_eq!(scalar.shape(), [1, 1]);
    assert_eq!(scalar[[0, 0]], 7);

    let empty = DenseArrayND::<u8, 2>::new([0, 4], |_| unreachable!("no element is generated")).expect("empty shape");
    assert_eq!(empty.reshaped([3, 0]).expect("empty shapes reshape").shape(), [3, 0]);

    let error = grid.reshaped([4, 2]).unwrap_err();
    assert_eq!(error, CollectionError::ElementCountMismatch { shape: 8, elements: 6 });
}

#[test]
fn one_read_algorithm_consumes_array_and_view() {
    fn column_sums<A: Array2D<i32>>(array: &A) -> Vec<i32> {
        let [rows, columns] = array.shape();
        let mut sums = vec![0; columns];
        for row in 0..rows {
            for (column, sum) in sums.iter_mut().enumerate() {
                *sum += array.get([row, column]).copied().unwrap_or(0);
            }
        }
        sums
    }

    let grid = DenseArrayND::new([2, 3], |[row, column]| (row * 3 + column) as i32).expect("shape fits");
    assert_eq!(column_sums(&grid), vec![3, 5, 7]);
    // 同一算法直接消费转置视图，不需要重写，也不要求视图连续。
    assert_eq!(column_sums(&grid.view().transpose()), vec![3, 12]);

    let mut mutable = grid.clone();
    assert_eq!(column_sums(&mutable.view_mut()), vec![3, 5, 7]);

    // 同一算法也直接消费编译期形状的内联表示，无需重写。
    let inline = InlineArray2D::<i32, 2, 3>::new(|[row, column]| (row * 3 + column) as i32);
    assert_eq!(column_sums(&inline), vec![3, 5, 7]);
}
