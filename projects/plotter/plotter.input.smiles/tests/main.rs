//! SMILES 前端冒烟：分子与反应式降级、立体信息记命名损失、缺片段整例拒绝。

use plotter_depiction::spec::{BondOrder, Element};
use plotter_input_smiles::{StructureSpec, smiles_to_scene, smiles_to_spec};
use plotter_types::codes;

#[test]
fn ethanol_lowers_to_one_three_atom_fragment() {
    let lowered = smiles_to_spec("CCO").expect("ethanol lowers");
    let StructureSpec::Molecule(molecule) = &lowered.spec
    else {
        panic!("a plain source is a molecule");
    };
    assert_eq!(molecule.atoms.len(), 3);
    assert_eq!(molecule.bonds.len(), 2);
    assert_eq!(molecule.atoms[2].element, Element::O);
    assert!(molecule.bonds.iter().all(|bond| bond.order == BondOrder::Single));
    assert!(lowered.diagnostics.is_empty(), "{:?}", lowered.diagnostics);
}

#[test]
fn benzene_keeps_all_six_aromatic_bonds() {
    let lowered = smiles_to_spec("c1ccccc1").expect("benzene lowers");
    let StructureSpec::Molecule(molecule) = &lowered.spec
    else {
        panic!("molecule");
    };
    assert_eq!(molecule.atoms.len(), 6);
    assert_eq!(molecule.bonds.len(), 6);
    assert!(molecule.bonds.iter().all(|bond| bond.order == BondOrder::Aromatic));
}

#[test]
fn bracket_atoms_keep_isotope_charge_and_hydrogens() {
    let lowered = smiles_to_spec("[13CH3-]").expect("bracket atom lowers");
    let StructureSpec::Molecule(molecule) = &lowered.spec
    else {
        panic!("molecule");
    };
    let atom = &molecule.atoms[0];
    assert_eq!(atom.element, Element::C);
    assert_eq!(atom.isotope, Some(13));
    assert_eq!(atom.hydrogens, Some(3));
    assert_eq!(atom.charge, -1);
}

#[test]
fn unknown_elements_stay_as_labelled_symbols() {
    let lowered = smiles_to_spec("[Na+]").expect("sodium lowers");
    let StructureSpec::Molecule(molecule) = &lowered.spec
    else {
        panic!("molecule");
    };
    assert_eq!(molecule.atoms[0].element, Element::Other("Na".to_string()));
}

#[test]
fn reaction_smiles_lower_to_reactants_products_and_arrow() {
    let lowered = smiles_to_spec("CCO.O>>CC=O").expect("reaction lowers");
    let StructureSpec::Reaction(reaction) = &lowered.spec
    else {
        panic!("an arrow makes a reaction");
    };
    assert_eq!(reaction.reactants.len(), 2);
    assert_eq!(reaction.products.len(), 1);
    assert!(reaction.products[0].bonds.iter().any(|bond| bond.order == BondOrder::Double));
}

#[test]
fn stereo_and_any_bonds_record_named_losses() {
    let stereo = smiles_to_spec("C/C=C/C").expect("stereo lowers");
    assert!(
        stereo.diagnostics.iter().any(|diagnostic| diagnostic.code().as_str() == codes::DEPICTION_APPROXIMATED),
        "stereo must be reported"
    );

    let any = smiles_to_spec("C~C").expect("any bond lowers");
    let StructureSpec::Molecule(molecule) = &any.spec
    else {
        panic!("molecule");
    };
    assert_eq!(molecule.bonds[0].order, BondOrder::Single);
    assert!(any.diagnostics.iter().any(|diagnostic| diagnostic.code().as_str() == codes::DEPICTION_APPROXIMATED));
}

#[test]
fn a_reaction_without_products_is_rejected() {
    let error = smiles_to_spec(">>CC").expect_err("one-sided reaction is refused");
    assert_eq!(error.code(), codes::DEPICTION_UNSUPPORTED);
}

#[test]
fn fused_purine_and_hypervalent_sulfur_lower_and_depict() {
    // 咖啡因的吡咯型 N 与 DMSO 的超价 S 曾被 1.5 均摊价校验整例拒绝。
    smiles_to_scene("Cn1cnc2c1c(=O)n(C)c(=O)n2C").expect("caffeine depicts");
    smiles_to_scene("CS(=O)C").expect("DMSO depicts");
    smiles_to_scene("c1ccc2ccccc2c1").expect("naphthalene depicts");
    smiles_to_scene("c1ccc2[nH]ccc2c1").expect("indole depicts");
}

#[test]
fn a_molecule_lowers_to_a_non_empty_scene() {
    let scene = smiles_to_scene("CCO").expect("ethanol depicts");
    assert!(scene.scene.size.width > 0.0);
    assert!(scene.scene.size.height > 0.0);
}
