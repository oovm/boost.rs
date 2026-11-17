//! Compile-time check that facade-only consumers can use `boost::collection::array!`.

use boost::collection::array;

type Grid = array!(f32, (3, 3));
type Cube = array!(f32, (2, 2, 2));

#[test]
fn facade_collection_macro_uses_the_facade_path() {
    assert_eq!(Grid::filled(0.0).shape(), [3, 3]);
    assert_eq!(Grid::new(|[row, column]| (row * 3 + column) as f32)[[2, 1]], 7.0);
    assert_eq!(Cube::filled(0.0).shape(), [2, 2, 2]);
    assert_eq!(Cube::new(|[first, second, third]| (first * 4 + second * 2 + third) as f32)[[1, 1, 1]], 7.0);
}
