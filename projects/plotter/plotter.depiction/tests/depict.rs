//! 分子与反应式的描绘冒烟测试。
//!
//! 所有夹具在测试内构造，不落盘、不入库二进制。

use plotter_depiction::{
    codes, molecule_to_scene_with_options,
    plan::{DepictionOptions, Palette},
    reaction_to_scene_with_options,
    spec::{ArrowKind, Atom, Bond, BondOrder, Element, MoleculeSpec, ReactionSpec},
};

fn carbon() -> Atom {
    Atom::new(Element::C)
}

fn atom_of(element: Element) -> Atom {
    Atom::new(element)
}

/// 苯：六元芳香环。
fn benzene() -> MoleculeSpec {
    let mut spec = MoleculeSpec::new();
    for _ in 0..6 {
        spec.push_atom(carbon());
    }
    for index in 0..6 {
        spec.push_bond(Bond::new(index, (index + 1) % 6, BondOrder::Aromatic));
    }
    spec
}

/// 萘：两个六元环共享一条键。
fn naphthalene() -> MoleculeSpec {
    let mut spec = MoleculeSpec::new();
    for _ in 0..10 {
        spec.push_atom(carbon());
    }
    for index in 0..6 {
        spec.push_bond(Bond::new(index, (index + 1) % 6, BondOrder::Aromatic));
    }
    // 第二个环 0-6-7-8-9-5-0，与第一个环共享 0-5 键。
    spec.push_bond(Bond::new(0, 6, BondOrder::Aromatic));
    spec.push_bond(Bond::new(6, 7, BondOrder::Aromatic));
    spec.push_bond(Bond::new(7, 8, BondOrder::Aromatic));
    spec.push_bond(Bond::new(8, 9, BondOrder::Aromatic));
    spec.push_bond(Bond::new(9, 5, BondOrder::Aromatic));
    spec
}

#[test]
fn benzene_draws_six_aromatic_rings_with_no_carbon_labels() {
    let lowered = molecule_to_scene_with_options(&benzene(), &DepictionOptions::default()).expect("benzene depicts");

    // 每条芳香键 = 一条实线 + 一条内侧虚线。
    assert_eq!(lowered.plan.lines.len(), 12);
    assert!(lowered.plan.lines.iter().any(|line| line.dashed), "the inner aromatic line is dashed");
    assert!(lowered.plan.labels.is_empty(), "carbon skeleton stays unlabelled");
    assert!(lowered.plan.diagnostics.is_empty(), "a plain ring reports no approximation");
    assert!(lowered.scene.size.width > 0.0 && lowered.scene.size.height > 0.0);
}

/// 甲苯：苯环外接一个取代基原子。
fn toluene() -> MoleculeSpec {
    let mut spec = benzene();
    let methyl = spec.push_atom(carbon());
    spec.push_bond(Bond::new(0, methyl, BondOrder::Single));
    spec
}

#[test]
fn a_ring_substituent_terminates_and_gets_a_position() {
    let scene = molecule_to_scene_with_options(&toluene(), &DepictionOptions::default()).expect("a substituted ring depicts without looping");
    assert!(scene.scene.size.width > 0.0);
    assert!(scene.scene.size.height > 0.0);
}

#[test]
fn a_ring_substituent_chain_keeps_bond_lengths() {
    let mut spec = benzene();
    let carbon = spec.push_atom(carbon());
    spec.push_bond(Bond::new(0, carbon, BondOrder::Single));
    let double_oxygen = spec.push_atom(atom_of(Element::O));
    spec.push_bond(Bond::new(carbon, double_oxygen, BondOrder::Double));
    let hydroxyl = spec.push_atom(atom_of(Element::O));
    spec.push_bond(Bond::new(carbon, hydroxyl, BondOrder::Single));

    let options = DepictionOptions::default();
    let lowered = molecule_to_scene_with_options(&spec, &options).expect("benzoic acid depicts");
    for line in &lowered.plan.lines {
        let length = line.from.distance(line.to);
        assert!(
            length <= options.bond_length * 1.2,
            "a substituent bond is {length} long, over one bond length; the chain was detached from the ring"
        );
    }
}

#[test]
fn benzene_vertices_are_one_bond_length_apart() {
    let options = DepictionOptions { palette: Palette::Ink, ..DepictionOptions::default() };
    let lowered = molecule_to_scene_with_options(&benzene(), &options).expect("benzene depicts");
    // 实线（未虚线的）就是六条骨架键。
    let skeleton: Vec<_> = lowered.plan.lines.iter().filter(|line| !line.dashed).collect();
    assert_eq!(skeleton.len(), 6);
    for line in skeleton {
        let length = line.from.distance(line.to);
        assert!((length - options.bond_length).abs() < 1.0, "bond length {length} deviates from {}", options.bond_length);
    }
}

#[test]
fn fused_rings_place_all_ten_atoms_without_collisions() {
    let options = DepictionOptions { palette: Palette::Ink, ..DepictionOptions::default() };
    let lowered = molecule_to_scene_with_options(&naphthalene(), &options).expect("naphthalene depicts");

    // 用骨架线的端点反推 10 个不同的原子位置。
    let mut points: Vec<(f64, f64)> = Vec::new();
    for line in lowered.plan.lines.iter().filter(|line| !line.dashed) {
        for point in [line.from, line.to] {
            let key = ((point.x * 100.0).round(), (point.y * 100.0).round());
            if !points.contains(&key) {
                points.push(key);
            }
        }
    }
    assert_eq!(points.len(), 10, "ten distinct ring vertices");

    for (index, first) in points.iter().enumerate() {
        for second in points.iter().skip(index + 1) {
            let distance = ((first.0 - second.0).powi(2) + (first.1 - second.1).powi(2)).sqrt() / 100.0;
            assert!(distance > 0.4 * options.bond_length, "fused vertices collide at {distance}");
        }
    }
}

#[test]
fn heteroatoms_are_labelled_and_carbon_is_not() {
    let mut spec = MoleculeSpec::new();
    let c = spec.push_atom(carbon());
    let o = spec.push_atom(atom_of(Element::O));
    spec.push_bond(Bond::new(c, o, BondOrder::Single));

    let lowered = molecule_to_scene_with_options(&spec, &DepictionOptions::default()).expect("methanol depicts");
    assert_eq!(lowered.plan.labels.len(), 1);
    assert_eq!(lowered.plan.labels[0].text, "OH", "the implicit hydrogen joins the oxygen label");
    assert!(lowered.plan.labels[0].backdrop, "label masks the bond it sits on");
}

#[test]
fn charge_and_isotope_appear_in_the_label() {
    let mut spec = MoleculeSpec::new();
    let mut nitrogen = atom_of(Element::N);
    nitrogen.charge = 1;
    let n = spec.push_atom(nitrogen);
    let mut carbon_atom = carbon();
    carbon_atom.isotope = Some(13);
    let c = spec.push_atom(carbon_atom);
    spec.push_bond(Bond::new(n, c, BondOrder::Single));

    let lowered = molecule_to_scene_with_options(&spec, &DepictionOptions::default()).expect("depicts");
    let texts: Vec<&str> = lowered.plan.labels.iter().map(|label| label.text.as_str()).collect();
    assert!(texts.contains(&"NH3+"), "the charged nitrogen keeps its inferred hydrogens and charge");
    assert!(texts.contains(&"13CH3"), "the isotope carbon keeps its inferred hydrogens");
}

#[test]
fn a_bondless_water_molecule_labels_hydrogens_first() {
    let mut spec = MoleculeSpec::new();
    spec.push_atom(atom_of(Element::O));
    let lowered = molecule_to_scene_with_options(&spec, &DepictionOptions::default()).expect("water depicts");
    assert_eq!(lowered.plan.labels[0].text, "H2O");
}

#[test]
fn the_element_palette_colors_heteroatom_labels_and_keeps_bonds_neutral() {
    let mut spec = MoleculeSpec::new();
    let c = spec.push_atom(carbon());
    let o = spec.push_atom(atom_of(Element::O));
    spec.push_bond(Bond::new(c, o, BondOrder::Single));

    let colored = molecule_to_scene_with_options(&spec, &DepictionOptions::default()).expect("ethanol depicts");
    let label = colored.plan.labels.first().expect("the oxygen is labelled");
    let color = label.color.expect("the element palette names a label colour");
    assert!(color.red() > 0xC0 && color.green() < 0x40, "oxygen is red: {color:?}");
    assert!(colored.plan.lines.iter().all(|line| line.color.is_none()), "skeletal bonds stay ink-black");

    let ink = DepictionOptions { palette: Palette::Ink, ..DepictionOptions::default() };
    let plain = molecule_to_scene_with_options(&spec, &ink).expect("ink ethanol depicts");
    assert!(plain.plan.labels.iter().all(|label| label.color.is_none()));
    assert!(plain.plan.lines.iter().all(|line| line.color.is_none()));
}

#[test]
fn two_rings_joined_by_a_single_bond_do_not_overlap() {
    // 联苯形：两个苯环以一条单键相连。第二个环若留在原点会与第一个环完全重叠。
    let mut spec = MoleculeSpec::new();
    for _ in 0..12 {
        spec.push_atom(carbon());
    }
    for index in 0..6 {
        spec.push_bond(Bond::new(index, (index + 1) % 6, BondOrder::Aromatic));
    }
    for index in 6..12 {
        spec.push_bond(Bond::new(index, 6 + (index + 1 - 6) % 6, BondOrder::Aromatic));
    }
    spec.push_bond(Bond::new(0, 6, BondOrder::Single));

    let options = DepictionOptions::default();
    let lowered = molecule_to_scene_with_options(&spec, &options).expect("biphenyl depicts");
    let mut points: Vec<(f64, f64)> = Vec::new();
    for line in lowered.plan.lines.iter().filter(|line| !line.dashed) {
        for point in [line.from, line.to] {
            let key = ((point.x * 10.0).round(), (point.y * 10.0).round());
            if !points.contains(&key) {
                points.push(key);
            }
        }
    }
    assert_eq!(points.len(), 12, "twelve distinct ring vertices, no overlap");
    for line in lowered.plan.lines.iter().filter(|line| !line.dashed) {
        let length = line.from.distance(line.to);
        assert!(length <= options.bond_length * 1.2, "bond of {length} leaves the one-bond-length band");
    }
}

#[test]
fn explicit_hydrogens_join_the_label() {
    let mut spec = MoleculeSpec::new();
    let mut oxygen = atom_of(Element::O);
    oxygen.hydrogens = Some(1);
    let o = spec.push_atom(oxygen);
    let c = spec.push_atom(carbon());
    spec.push_bond(Bond::new(c, o, BondOrder::Single));

    let lowered = molecule_to_scene_with_options(&spec, &DepictionOptions::default()).expect("depicts");
    assert_eq!(lowered.plan.labels[0].text, "OH");
}

#[test]
fn double_and_triple_bonds_draw_parallel_lines() {
    let mut double = MoleculeSpec::new();
    let a = double.push_atom(carbon());
    let b = double.push_atom(carbon());
    double.push_bond(Bond::new(a, b, BondOrder::Double));
    let lowered = molecule_to_scene_with_options(&double, &DepictionOptions::default()).expect("depicts");
    assert_eq!(lowered.plan.lines.len(), 2);

    let mut triple = MoleculeSpec::new();
    let a = triple.push_atom(carbon());
    let b = triple.push_atom(carbon());
    triple.push_bond(Bond::new(a, b, BondOrder::Triple));
    let lowered = molecule_to_scene_with_options(&triple, &DepictionOptions::default()).expect("depicts");
    assert_eq!(lowered.plan.lines.len(), 3);
}

#[test]
fn over_valent_atom_is_refused() {
    let mut spec = MoleculeSpec::new();
    let c = spec.push_atom(carbon());
    for _ in 0..5 {
        let h = spec.push_atom(atom_of(Element::H));
        spec.push_bond(Bond::new(c, h, BondOrder::Single));
    }
    let error = molecule_to_scene_with_options(&spec, &DepictionOptions::default()).expect_err("five bonds on carbon is refused");
    assert_eq!(error.code(), codes::DEPICTION_UNSUPPORTED);
    assert!(error.message().contains("bond order units"), "refusal names the valence arithmetic");
}

#[test]
fn self_bond_and_duplicate_bond_are_refused() {
    let mut spec = MoleculeSpec::new();
    spec.push_atom(carbon());
    spec.push_bond(Bond::new(0, 0, BondOrder::Single));
    assert_eq!(
        molecule_to_scene_with_options(&spec, &DepictionOptions::default()).expect_err("self bond").code(),
        codes::DEPICTION_UNSUPPORTED
    );

    let mut spec = MoleculeSpec::new();
    spec.push_atom(carbon());
    spec.push_atom(carbon());
    spec.push_bond(Bond::new(0, 1, BondOrder::Single));
    spec.push_bond(Bond::new(1, 0, BondOrder::Single));
    assert_eq!(
        molecule_to_scene_with_options(&spec, &DepictionOptions::default()).expect_err("duplicate bond").code(),
        codes::DEPICTION_UNSUPPORTED
    );
}

#[test]
fn empty_molecule_is_refused() {
    let error = molecule_to_scene_with_options(&MoleculeSpec::new(), &DepictionOptions::default()).expect_err("empty");
    assert_eq!(error.code(), codes::DEPICTION_UNSUPPORTED);
}

#[test]
fn disconnected_fragment_is_shifted_and_reported() {
    let mut spec = MoleculeSpec::new();
    let first = spec.push_atom(atom_of(Element::O));
    let second = spec.push_atom(atom_of(Element::O));
    // 两个孤立原子构成两个连通分量。
    let _ = (first, second);

    let lowered = molecule_to_scene_with_options(&spec, &DepictionOptions::default()).expect("salt-like input depicts");
    let approximated: Vec<_> = lowered.diagnostics.iter().filter(|d| d.code().as_str() == codes::DEPICTION_APPROXIMATED).collect();
    assert_eq!(approximated.len(), 1);
    assert!(approximated[0].message().to_string().contains("disconnected"));
    assert_eq!(lowered.plan.labels.len(), 2, "both oxygen atoms are labelled");
}

#[test]
fn scene_serializes_to_svg_with_edges_and_labels() {
    let mut spec = MoleculeSpec::new();
    let c = spec.push_atom(carbon());
    let o = spec.push_atom(atom_of(Element::O));
    spec.push_bond(Bond::new(c, o, BondOrder::Double));

    let lowered = molecule_to_scene_with_options(&spec, &DepictionOptions::default()).expect("depicts");
    let svg = lowered.scene.to_svg().expect("scene serializes");
    assert!(svg.starts_with("<svg"));
    assert!(svg.contains("data-role=\"edge\""), "bonds carry the edge role");
    assert!(svg.contains("data-role=\"label\""), "the oxygen label is emitted");
}

#[test]
fn reaction_emits_plus_sign_arrow_and_conditions() {
    let mut water = MoleculeSpec::new();
    let o = water.push_atom(atom_of(Element::O));
    let mut hydrogen = MoleculeSpec::new();
    hydrogen.push_atom(atom_of(Element::H));
    let mut product = MoleculeSpec::new();
    let _ = product.push_atom(atom_of(Element::O));

    let mut reaction = ReactionSpec::new(vec![water, hydrogen], vec![product]);
    reaction.above = Some("cat.".to_string());
    reaction.below = Some("25 C".to_string());
    reaction.arrow = ArrowKind::Equilibrium;

    let lowered = reaction_to_scene_with_options(&reaction, &DepictionOptions::default()).expect("reaction depicts");
    assert_eq!(lowered.plan.arrows.len(), 1);
    let arrow = &lowered.plan.arrows[0];
    assert_eq!(arrow.above.as_ref().map(|label| label.text.as_str()), Some("cat."));
    assert_eq!(arrow.below.as_ref().map(|label| label.text.as_str()), Some("25 C"));
    assert_eq!(arrow.kind, ArrowKind::Equilibrium);
    assert!(lowered.plan.labels.iter().any(|label| label.text == "+"), "one plus sign between reactants");
    assert!(lowered.plan.arrows[0].to.x > lowered.plan.arrows[0].from.x, "arrow points right");
    let _ = o;
}

#[test]
fn reaction_without_products_is_refused() {
    let mut water = MoleculeSpec::new();
    water.push_atom(atom_of(Element::O));
    let reaction = ReactionSpec::new(vec![water], Vec::new());
    let error = reaction_to_scene_with_options(&reaction, &DepictionOptions::default()).expect_err("needs products");
    assert_eq!(error.code(), codes::DEPICTION_UNSUPPORTED);
}

#[test]
fn chain_atoms_zigzag_without_overlap() {
    let mut spec = MoleculeSpec::new();
    let mut previous = spec.push_atom(carbon());
    for _ in 0..5 {
        let next = spec.push_atom(carbon());
        spec.push_bond(Bond::new(previous, next, BondOrder::Single));
        previous = next;
    }

    let options = DepictionOptions { palette: Palette::Ink, ..DepictionOptions::default() };
    let lowered = molecule_to_scene_with_options(&spec, &options).expect("hexane skeleton depicts");
    assert_eq!(lowered.plan.lines.len(), 5);
    for line in &lowered.plan.lines {
        let length = line.from.distance(line.to);
        assert!((length - options.bond_length).abs() < 1.0, "chain bond keeps the bond length, got {length}");
    }
}
