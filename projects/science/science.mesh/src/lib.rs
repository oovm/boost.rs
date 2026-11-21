#![deny(missing_docs)]
//! Unstructured mesh with per-node fields, plus deterministic VTK and CSV output.
//!
//! OpenCAE and OpenCFD both write meshes and result fields. This crate keeps one
//! cell vocabulary and one writer, so a result opened in ParaView carries the
//! same cell codes no matter which tool produced it.
//!
//! Output is byte-stable for a given mesh: points, cells, and fields are written
//! in their stored order, and numbers use Rust's shortest round-trip form.

use std::fmt;

/// Cell topologies the writers understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    /// A single point cell.
    Vertex,
    /// A two-node line.
    Line,
    /// A three-node triangle.
    Triangle,
    /// A four-node quadrilateral.
    Quad,
    /// A four-node tetrahedron.
    Tetra,
    /// An eight-node hexahedron.
    Hexahedron,
}

impl CellKind {
    /// Number of nodes one cell of this kind carries.
    pub fn node_count(self) -> usize {
        match self {
            Self::Vertex => 1,
            Self::Line => 2,
            Self::Triangle => 3,
            Self::Quad | Self::Tetra => 4,
            Self::Hexahedron => 8,
        }
    }

    /// The legacy VTK cell-type code.
    pub fn vtk_code(self) -> u8 {
        match self {
            Self::Vertex => 1,
            Self::Line => 3,
            Self::Triangle => 5,
            Self::Quad => 9,
            Self::Tetra => 10,
            Self::Hexahedron => 12,
        }
    }
}

/// One cell and the point indices it references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// Cell topology.
    pub kind: CellKind,
    /// Point indices in the order the topology defines.
    pub nodes: Vec<usize>,
}

impl Cell {
    /// Create a cell.
    pub fn new(kind: CellKind, nodes: Vec<usize>) -> Self {
        Self { kind, nodes }
    }
}

/// A point cloud plus its cells.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mesh {
    /// Point coordinates in `[x, y, z]` order.
    pub points: Vec<[f64; 3]>,
    /// Cells in declaration order.
    pub cells: Vec<Cell>,
}

impl Mesh {
    /// Create a mesh from points and cells.
    pub fn new(points: Vec<[f64; 3]>, cells: Vec<Cell>) -> Self {
        Self { points, cells }
    }

    /// Number of points.
    pub fn point_count(&self) -> usize {
        self.points.len()
    }

    /// Number of cells.
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// Check indices, node counts, and coordinate finiteness.
    pub fn validate(&self) -> Result<(), MeshError> {
        for (index, point) in self.points.iter().enumerate() {
            if !point.iter().all(|value| value.is_finite()) {
                return Err(MeshError::new("mesh/point", format!("point {index} is not finite")));
            }
        }
        for (index, cell) in self.cells.iter().enumerate() {
            if cell.nodes.len() != cell.kind.node_count() {
                return Err(MeshError::new(
                    "mesh/cell-arity",
                    format!("cell {index} is {:?} and needs {} nodes, found {}", cell.kind, cell.kind.node_count(), cell.nodes.len()),
                ));
            }
            for node in &cell.nodes {
                if *node >= self.points.len() {
                    return Err(MeshError::new(
                        "mesh/cell-index",
                        format!("cell {index} references point {node} but the mesh has {} points", self.points.len()),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Validate every field against the point count.
    pub fn validate_fields(&self, fields: &[Field]) -> Result<(), MeshError> {
        for field in fields {
            if field.components == 0 {
                return Err(MeshError::new("mesh/field-components", format!("field `{}` has zero components", field.name)));
            }
            if field.len() != self.points.len() {
                return Err(MeshError::new(
                    "mesh/field-length",
                    format!("field `{}` has {} tuples but the mesh has {} points", field.name, field.len(), self.points.len()),
                ));
            }
            if !field.values.iter().all(|value| value.is_finite()) {
                return Err(MeshError::new("mesh/field-value", format!("field `{}` contains a non-finite value", field.name)));
            }
        }
        Ok(())
    }

    /// Serialize as a legacy ASCII VTK unstructured grid with point data.
    pub fn to_legacy_vtk(&self, fields: &[Field]) -> Result<String, MeshError> {
        self.validate()?;
        self.validate_fields(fields)?;
        let mut out = String::from("# vtk DataFile Version 3.0\nOpenMesh\nASCII\nDATASET UNSTRUCTURED_GRID\n");
        out.push_str(&format!("POINTS {} double\n", self.points.len()));
        for point in &self.points {
            out.push_str(&format!("{} {} {}\n", number(point[0]), number(point[1]), number(point[2])));
        }
        let flattened: usize = self.cells.iter().map(|cell| cell.nodes.len() + 1).sum();
        out.push_str(&format!("CELLS {} {flattened}\n", self.cells.len()));
        for cell in &self.cells {
            out.push_str(&format!("{}", cell.nodes.len()));
            for node in &cell.nodes {
                out.push_str(&format!(" {node}"));
            }
            out.push('\n');
        }
        out.push_str(&format!("CELL_TYPES {}\n", self.cells.len()));
        for cell in &self.cells {
            out.push_str(&format!("{}\n", cell.kind.vtk_code()));
        }
        if !fields.is_empty() {
            out.push_str(&format!("POINT_DATA {}\n", self.points.len()));
            for field in fields {
                write_field(&mut out, field);
            }
        }
        Ok(out)
    }

    /// Serialize point coordinates and fields as CSV.
    ///
    /// Scalars add one column named after the field; vectors add one column per
    /// component suffixed `_x`, `_y`, and `_z` (or `_0`, `_1`, ...).
    pub fn to_csv(&self, fields: &[Field]) -> Result<String, MeshError> {
        self.validate_fields(fields)?;
        let mut header = String::from("x,y,z");
        for field in fields {
            if field.components == 1 {
                header.push(',');
                header.push_str(&field.name);
            } else {
                for component in 0..field.components {
                    header.push(',');
                    header.push_str(&field.name);
                    header.push('_');
                    header.push_str(component_suffix(component));
                }
            }
        }
        header.push('\n');
        let mut out = header;
        for (index, point) in self.points.iter().enumerate() {
            out.push_str(&format!("{},{},{}", number(point[0]), number(point[1]), number(point[2])));
            for field in fields {
                let start = index * field.components;
                for component in 0..field.components {
                    out.push(',');
                    out.push_str(&number(field.values[start + component]));
                }
            }
            out.push('\n');
        }
        Ok(out)
    }
}

/// A per-point field.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// Field name as it appears in the output.
    pub name: String,
    /// Values per point.
    pub components: usize,
    /// Flattened values, `components` numbers per point.
    pub values: Vec<f64>,
}

impl Field {
    /// A scalar field.
    pub fn scalar(name: impl Into<String>, values: Vec<f64>) -> Self {
        Self { name: name.into(), components: 1, values }
    }

    /// A three-component vector field.
    pub fn vector(name: impl Into<String>, values: Vec<f64>) -> Self {
        Self { name: name.into(), components: 3, values }
    }

    /// Number of tuples, computed from the flattened values.
    pub fn len(&self) -> usize {
        self.values.len().checked_div(self.components).unwrap_or(0)
    }

    /// Whether the field holds no value.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// A mesh or field failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshError {
    code: &'static str,
    message: String,
}

impl MeshError {
    /// Create an error from a stable code and a message.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    /// The stable failure code.
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The failure description.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for MeshError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for MeshError {}

fn write_field(out: &mut String, field: &Field) {
    if field.components == 1 {
        out.push_str(&format!("SCALARS {} double 1\nLOOKUP_TABLE default\n", field.name));
        for value in &field.values {
            out.push_str(&number(*value));
            out.push('\n');
        }
    } else {
        out.push_str(&format!("VECTORS {} double\n", field.name));
        for tuple in field.values.chunks(field.components) {
            let mut parts = Vec::with_capacity(tuple.len());
            for value in tuple {
                parts.push(number(*value));
            }
            out.push_str(&parts.join(" "));
            out.push('\n');
        }
    }
}

fn component_suffix(component: usize) -> &'static str {
    match component {
        0 => "x",
        1 => "y",
        2 => "z",
        _ => "n",
    }
}

fn number(value: f64) -> String {
    format!("{value:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triangle_mesh() -> Mesh {
        Mesh::new(
            vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            vec![Cell::new(CellKind::Triangle, vec![0, 1, 2])],
        )
    }

    #[test]
    fn legacy_vtk_has_expected_sections() {
        let mesh = triangle_mesh();
        let field = Field::scalar("temperature", vec![1.0, 2.0, 3.0]);
        let vtk = mesh.to_legacy_vtk(&[field]).expect("vtk");
        assert!(vtk.starts_with("# vtk DataFile Version 3.0\n"));
        assert!(vtk.contains("POINTS 3 double\n0.0 0.0 0.0\n"));
        assert!(vtk.contains("CELLS 1 4\n3 0 1 2\n"));
        assert!(vtk.contains("CELL_TYPES 1\n5\n"));
        assert!(vtk.contains("POINT_DATA 3\nSCALARS temperature double 1\nLOOKUP_TABLE default\n"));
        assert!(!vtk.contains("VECTORS"));
    }

    #[test]
    fn vtk_output_is_deterministic() {
        let mesh = triangle_mesh();
        let field = Field::vector("velocity", vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
        let first = mesh.to_legacy_vtk(std::slice::from_ref(&field)).expect("first");
        let second = mesh.to_legacy_vtk(&[field]).expect("second");
        assert_eq!(first, second);
        assert!(first.contains("VECTORS velocity double\n0.0 0.0 0.0\n1.0 0.0 0.0\n0.0 1.0 0.0\n"));
    }

    #[test]
    fn csv_columns_follow_field_shape() {
        let mesh = triangle_mesh();
        let pressure = Field::scalar("p", vec![10.0, 20.0, 30.0]);
        let velocity = Field::vector("u", vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
        let csv = mesh.to_csv(&[pressure, velocity]).expect("csv");
        let mut lines = csv.lines();
        assert_eq!(lines.next(), Some("x,y,z,p,u_x,u_y,u_z"));
        assert_eq!(lines.next(), Some("0.0,0.0,0.0,10.0,1.0,2.0,3.0"));
        assert_eq!(lines.count(), 2);
    }

    #[test]
    fn validation_rejects_bad_indices_and_arity() {
        let mut mesh = triangle_mesh();
        mesh.cells[0].nodes = vec![0, 1, 9];
        let error = mesh.validate().expect_err("index");
        assert_eq!(error.code(), "mesh/cell-index");

        let mut mesh = triangle_mesh();
        mesh.cells[0].nodes = vec![0, 1];
        let error = mesh.validate().expect_err("arity");
        assert_eq!(error.code(), "mesh/cell-arity");
    }

    #[test]
    fn validation_rejects_field_length_mismatch() {
        let mesh = triangle_mesh();
        let error = mesh.to_legacy_vtk(&[Field::scalar("p", vec![1.0])]).expect_err("length");
        assert_eq!(error.code(), "mesh/field-length");
    }
}
