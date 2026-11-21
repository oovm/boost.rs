#![deny(missing_docs)]
//! Planar geometry in `f64`.
//!
//! The OpenCAD, OpenPCB, and OpenEDA tools all need the same two-dimensional
//! primitives: point/segment distance, segment intersection, polygon tests, and
//! arc sampling. This crate owns them once so the three tools cannot drift.
//!
//! Coordinates are in whatever planar unit the caller uses; the crate only
//! assumes that x grows to the right and y grows upward unless a caller states
//! otherwise. Angles are radians, counter-clockwise from the positive x axis.

use std::fmt;
use std::ops::{Add, Mul, Sub};

/// A point or vector in the plane.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Point {
    /// Horizontal coordinate.
    pub x: f64,
    /// Vertical coordinate.
    pub y: f64,
}

impl Point {
    /// The origin.
    pub const ORIGIN: Self = Self { x: 0.0, y: 0.0 };

    /// Create a point.
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// Euclidean distance to another point.
    pub fn distance(self, other: Self) -> f64 {
        (self - other).length()
    }

    /// Euclidean length as a vector.
    pub fn length(self) -> f64 {
        self.x.hypot(self.y)
    }

    /// Dot product.
    pub fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y
    }

    /// Two-dimensional cross product.
    pub fn cross(self, other: Self) -> f64 {
        self.x * other.y - self.y * other.x
    }

    /// The point scaled by `factor`.
    pub fn scaled(self, factor: f64) -> Self {
        Self { x: self.x * factor, y: self.y * factor }
    }

    /// Whether both coordinates are finite.
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

impl Add for Point {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self { x: self.x + other.x, y: self.y + other.y }
    }
}

impl Sub for Point {
    type Output = Self;

    fn sub(self, other: Self) -> Self {
        Self { x: self.x - other.x, y: self.y - other.y }
    }
}

impl Mul<f64> for Point {
    type Output = Self;

    fn mul(self, factor: f64) -> Self {
        self.scaled(factor)
    }
}

/// An axis-aligned rectangle stored as two opposite corners.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Lower-left corner.
    pub min: Point,
    /// Upper-right corner.
    pub max: Point,
}

impl Rect {
    /// Create a normalized rectangle from two arbitrary corners.
    pub fn from_points(a: Point, b: Point) -> Self {
        Self {
            min: Point::new(a.x.min(b.x), a.y.min(b.y)),
            max: Point::new(a.x.max(b.x), a.y.max(b.y)),
        }
    }

    /// Horizontal extent.
    pub fn width(self) -> f64 {
        self.max.x - self.min.x
    }

    /// Vertical extent.
    pub fn height(self) -> f64 {
        self.max.y - self.min.y
    }

    /// Rectangle center.
    pub fn center(self) -> Point {
        Point::new((self.min.x + self.max.x) * 0.5, (self.min.y + self.max.y) * 0.5)
    }

    /// Whether the point lies inside or on the border.
    pub fn contains(self, point: Point) -> bool {
        point.x >= self.min.x && point.x <= self.max.x && point.y >= self.min.y && point.y <= self.max.y
    }

    /// Smallest rectangle covering both inputs.
    pub fn union(self, other: Self) -> Self {
        Self {
            min: Point::new(self.min.x.min(other.min.x), self.min.y.min(other.min.y)),
            max: Point::new(self.max.x.max(other.max.x), self.max.y.max(other.max.y)),
        }
    }

    /// Grow by `margin` on every side.
    pub fn expanded(self, margin: f64) -> Self {
        Self {
            min: Point::new(self.min.x - margin, self.min.y - margin),
            max: Point::new(self.max.x + margin, self.max.y + margin),
        }
    }
}

/// Distance from `point` to the segment `a`–`b`.
///
/// A degenerate segment (`a == b`) measures the distance to that point.
pub fn point_segment_distance(point: Point, a: Point, b: Point) -> f64 {
    let ab = b - a;
    let length_squared = ab.dot(ab);
    if length_squared == 0.0 {
        return point.distance(a);
    }
    let t = ((point - a).dot(ab) / length_squared).clamp(0.0, 1.0);
    point.distance(a + ab * t)
}

/// Distance between two points of two segments.
pub fn segment_distance(a: Point, b: Point, c: Point, d: Point) -> f64 {
    if segment_intersection(a, b, c, d).is_some() {
        return 0.0;
    }
    point_segment_distance(a, c, d)
        .min(point_segment_distance(b, c, d))
        .min(point_segment_distance(c, a, b))
        .min(point_segment_distance(d, a, b))
}

/// Intersection point of two segments, including touching endpoints.
///
/// Collinear overlapping segments have no single intersection point and return
/// `None`; use [`segment_distance`] to detect that overlap.
pub fn segment_intersection(a: Point, b: Point, c: Point, d: Point) -> Option<Point> {
    let r = b - a;
    let s = d - c;
    let denominator = r.cross(s);
    if denominator.abs() < 1e-12 {
        return None;
    }
    let offset = c - a;
    let t = offset.cross(s) / denominator;
    let u = offset.cross(r) / denominator;
    if (-1e-12..=1.0 + 1e-12).contains(&t) && (-1e-12..=1.0 + 1e-12).contains(&u) {
        Some(a + r * t)
    } else {
        None
    }
}

/// Whether a point lies inside a polygon, treating the border as inside.
pub fn point_in_polygon(point: Point, polygon: &[Point]) -> bool {
    if polygon.len() < 3 {
        return false;
    }
    for (index, start) in polygon.iter().enumerate() {
        let end = polygon[(index + 1) % polygon.len()];
        if point_segment_distance(point, *start, end) <= 1e-12 {
            return true;
        }
    }
    let mut inside = false;
    for (index, start) in polygon.iter().enumerate() {
        let end = polygon[(index + 1) % polygon.len()];
        let crosses = (start.y > point.y) != (end.y > point.y);
        if crosses {
            let x = (end.x - start.x) * (point.y - start.y) / (end.y - start.y) + start.x;
            if point.x < x {
                inside = !inside;
            }
        }
    }
    inside
}

/// Signed area; positive for counter-clockwise winding.
pub fn signed_polygon_area(polygon: &[Point]) -> f64 {
    let mut sum = 0.0;
    for (index, start) in polygon.iter().enumerate() {
        let end = polygon[(index + 1) % polygon.len()];
        sum += start.cross(end);
    }
    sum * 0.5
}

/// Absolute polygon area.
pub fn polygon_area(polygon: &[Point]) -> f64 {
    signed_polygon_area(polygon).abs()
}

/// Axis-aligned bounds of a point set, or `None` when it is empty.
pub fn bounds(points: &[Point]) -> Option<Rect> {
    let first = *points.first()?;
    Some(points.iter().skip(1).fold(Rect::from_points(first, first), |rect, point| rect.union(Rect::from_points(*point, *point))))
}

/// Sample an arc into `segments` straight segments.
///
/// The result always starts at `start_radians` and ends at `end_radians`, so the
/// endpoints are exact and the interior is a polyline approximation. At least
/// one segment is produced.
pub fn arc_points(center: Point, radius: f64, start_radians: f64, end_radians: f64, segments: usize) -> Vec<Point> {
    let segments = segments.max(1);
    (0..=segments)
        .map(|index| {
            let t = index as f64 / segments as f64;
            let angle = start_radians + (end_radians - start_radians) * t;
            Point::new(center.x + radius * angle.cos(), center.y + radius * angle.sin())
        })
        .collect()
}

/// A planar geometry failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeometryError {
    message: String,
}

impl GeometryError {
    /// Create an error from a description.
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }

    /// The failure description.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for GeometryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GeometryError {}

/// Reject a point that is not finite.
pub fn require_finite(point: Point, context: &str) -> Result<Point, GeometryError> {
    if point.is_finite() { Ok(point) } else { Err(GeometryError::new(format!("{context} must be finite, found ({}, {})", point.x, point.y))) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_distance_handles_projection_and_degenerate_input() {
        let a = Point::new(0.0, 0.0);
        let b = Point::new(10.0, 0.0);
        assert!((point_segment_distance(Point::new(5.0, 3.0), a, b) - 3.0).abs() < 1e-9);
        assert!((point_segment_distance(Point::new(-4.0, 0.0), a, b) - 4.0).abs() < 1e-9);
        assert!((point_segment_distance(Point::new(3.0, 4.0), a, a) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn crossing_segments_intersect_and_parallel_ones_do_not() {
        let hit = segment_intersection(Point::new(0.0, 0.0), Point::new(4.0, 4.0), Point::new(0.0, 4.0), Point::new(4.0, 0.0));
        let hit = hit.expect("crossing");
        assert!((hit.x - 2.0).abs() < 1e-9 && (hit.y - 2.0).abs() < 1e-9);
        assert!(segment_intersection(Point::new(0.0, 0.0), Point::new(1.0, 0.0), Point::new(0.0, 1.0), Point::new(1.0, 1.0)).is_none());
        assert!((segment_distance(Point::new(0.0, 0.0), Point::new(1.0, 0.0), Point::new(0.0, 1.0), Point::new(1.0, 1.0)) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn polygon_contains_interior_border_and_exterior() {
        let square = [Point::new(0.0, 0.0), Point::new(4.0, 0.0), Point::new(4.0, 4.0), Point::new(0.0, 4.0)];
        assert!(point_in_polygon(Point::new(2.0, 2.0), &square));
        assert!(point_in_polygon(Point::new(0.0, 2.0), &square));
        assert!(!point_in_polygon(Point::new(5.0, 2.0), &square));
        assert!(!point_in_polygon(Point::new(2.0, 2.0), &square[..2]));
    }

    #[test]
    fn polygon_area_follows_winding() {
        let square = [Point::new(0.0, 0.0), Point::new(4.0, 0.0), Point::new(4.0, 4.0), Point::new(0.0, 4.0)];
        assert!((polygon_area(&square) - 16.0).abs() < 1e-9);
        assert!((signed_polygon_area(&square) - 16.0).abs() < 1e-9);
        let reversed: Vec<Point> = square.iter().rev().copied().collect();
        assert!((signed_polygon_area(&reversed) + 16.0).abs() < 1e-9);
    }

    #[test]
    fn bounds_and_rect_helpers() {
        let rect = bounds(&[Point::new(1.0, 1.0), Point::new(-2.0, 5.0)]).expect("non-empty");
        assert_eq!(rect.min, Point::new(-2.0, 1.0));
        assert_eq!(rect.max, Point::new(1.0, 5.0));
        assert_eq!(rect.width(), 3.0);
        assert_eq!(rect.height(), 4.0);
        assert!(rect.contains(Point::new(0.0, 2.0)));
        assert!(bounds(&[]).is_none());
        let grown = rect.expanded(1.0);
        assert_eq!(grown.min, Point::new(-3.0, 0.0));
    }

    #[test]
    fn arc_sampling_hits_both_endpoints() {
        let points = arc_points(Point::new(0.0, 0.0), 2.0, 0.0, std::f64::consts::PI, 4);
        assert_eq!(points.len(), 5);
        assert!((points[0] - Point::new(2.0, 0.0)).length() < 1e-9);
        assert!((points[4] - Point::new(-2.0, 0.0)).length() < 1e-9);
        assert!((points[2] - Point::new(0.0, 2.0)).length() < 1e-9);
    }

    #[test]
    fn non_finite_points_are_rejected() {
        assert!(require_finite(Point::new(1.0, 2.0), "point").is_ok());
        assert!(require_finite(Point::new(f64::NAN, 2.0), "point").is_err());
    }
}
