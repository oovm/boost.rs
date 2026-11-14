//! MDL 前端冒烟：连接表降级、属性合并、坐标损失记录、越界整例拒绝。

use std::ops::Range;

use plotter_depiction::spec::{BondOrder, Element};
use plotter_input_mdl::{mdl_to_scene, mdl_to_spec};
use plotter_types::codes;

fn span_of<'a>(source: &'a str, error: &plotter_depiction::DepictionError) -> &'a str {
    let span = error.span().unwrap_or_else(|| panic!("`{}` carries no span", error.message()));
    let Range { start, end } = span.clone();
    assert!(end <= source.len(), "span {start}..{end} leaves a {} byte source", source.len());
    &source[start..end]
}

/// 一条水分子记录，三行头 + counts + 一个原子行。
fn water() -> String {
    let mut text = String::new();
    text.push_str("water\n");
    text.push_str("  oak     2D\n");
    text.push_str("\n");
    text.push_str("  1  0  0  0  0  0  0  0  0  0999 V2000\n");
    text.push_str("    0.0000    0.0000    0.0000 O   0  0  0  0  0  0  0  0  0  0  0  0\n");
    text
}

#[test]
fn water_lowers_to_one_oxygen_and_reports_the_layout_loss() {
    let lowered = mdl_to_spec(&water()).expect("water lowers");
    assert_eq!(lowered.spec.atoms.len(), 1);
    assert_eq!(lowered.spec.atoms[0].element, Element::O);
    assert!(
        lowered.diagnostics.iter().any(|diagnostic| diagnostic.code().as_str() == codes::DEPICTION_APPROXIMATED),
        "the authored coordinates are replaced, which must be reported"
    );
}

#[test]
fn benzene_bonds_lower_with_their_orders() {
    let mut text = String::new();
    text.push_str("benzene\n  oak\n\n");
    text.push_str("  6  6  0  0  0  0  0  0  0  0999 V2000\n");
    for index in 0..6 {
        let angle = std::f64::consts::TAU * index as f64 / 6.0;
        text.push_str(&format!(
            "{:10.4}{:10.4}{:10.4} C   0  0  0  0  0  0  0  0  0  0  0  0\n",
            angle.cos(),
            angle.sin(),
            0.0
        ));
    }
    for index in 0..6 {
        let from = index + 1;
        let to = (index + 1) % 6 + 1;
        text.push_str(&format!("{:3}{:3}{:3}{:3}  0  0  0  0  0  0  0\n", from, to, 4, 0));
    }

    let lowered = mdl_to_spec(&text).expect("benzene lowers");
    assert_eq!(lowered.spec.atoms.len(), 6);
    assert_eq!(lowered.spec.bonds.len(), 6);
    assert!(lowered.spec.bonds.iter().all(|bond| bond.order == BondOrder::Aromatic));
    assert_eq!((lowered.spec.bonds[0].from, lowered.spec.bonds[0].to), (0, 1), "1-based endpoints become 0-based");
}

#[test]
fn charge_and_isotope_properties_override_the_atom_line() {
    let mut text = water();
    text.push_str("M  CHG  1   1  -1\n");
    text.push_str("M  ISO  1   1  18\n");

    let lowered = mdl_to_spec(&text).expect("properties lower");
    let atom = &lowered.spec.atoms[0];
    assert_eq!(atom.charge, -1);
    assert_eq!(atom.isotope, Some(18));
}

#[test]
fn an_out_of_range_bond_endpoint_is_rejected_with_its_line_span() {
    let mut text = String::new();
    text.push_str("broken\n  oak\n\n");
    text.push_str("  1  1  0  0  0  0  0  0  0  0999 V2000\n");
    text.push_str("    0.0000    0.0000    0.0000 C   0  0  0  0  0  0  0  0  0  0  0  0\n");
    text.push_str("  1  2  1  0  0  0  0\n");

    let error = mdl_to_spec(&text).expect_err("endpoint 2 has no atom");
    assert_eq!(error.code(), codes::DEPICTION_UNSUPPORTED);
    assert!(span_of(&text, &error).contains("  1  2  1"));
}

#[test]
fn a_molecule_lowers_to_a_non_empty_scene() {
    let scene = mdl_to_scene(&water()).expect("water depicts");
    assert!(scene.scene.size.width > 0.0);
    assert!(scene.scene.size.height > 0.0);
}
