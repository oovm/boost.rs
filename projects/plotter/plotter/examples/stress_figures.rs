//! Scratch stress renderer for visual review. Not part of the crate contract.

use std::{env, fs, path::Path};

fn write(name: &str, scene: plotter::scene::DisplayScene, out: &Path) {
    let svg = scene.to_svg_with_authored_hooks(&format!("{name}-scene"), name, &plotter::types::DIAGRAM_THEME).expect("scene serializes");
    fs::write(out.join(format!("{name}.svg")), svg).expect("write svg");
}

fn chem(name: &str, smiles: &str, out: &Path) {
    match plotter::input::smiles::smiles_to_spec(smiles) {
        Ok(lowered) => {
            if let plotter::input::smiles::StructureSpec::Molecule(molecule) = &lowered.spec {
                if std::env::var("DUMP_POS").is_ok() {
                    let options = plotter::depiction::DepictionOptions::default();
                    let plan = plotter::depiction::depict_molecule(molecule, &options).expect("depict");
                    println!("{name}: canvas {:?}", plan.canvas);
                    for line in &plan.lines {
                        println!(
                            "  ({:.1},{:.1})-({:.1},{:.1}) len={:.1}",
                            line.from.x,
                            line.from.y,
                            line.to.x,
                            line.to.y,
                            line.from.distance(line.to)
                        );
                    }
                }
            }
            match plotter::input::smiles::smiles_to_scene(smiles) {
                Ok(scene) => write(name, scene.scene, out),
                Err(error) => println!("FAIL {name}: {}", error.message()),
            }
        }
        Err(error) => println!("FAIL {name}: {}", error.message()),
    }
}

fn main() {
    let out = Path::new("E:/tmp-render");
    fs::create_dir_all(out).expect("create output dir");
    let only = env::args().nth(1);

    let cases: &[(&str, &str)] = &[
        ("naphthalene", "c1ccc2ccccc2c1"),
        ("anthracene", "c1ccc2cc3ccccc3cc2c1"),
        ("indole", "c1ccc2[nH]ccc2c1"),
        ("caffeine", "Cn1cnc2c1c(=O)n(C)c(=O)n2C"),
        ("pyridine", "c1ccncc1"),
        ("furan", "c1ccoc1"),
        ("biphenyl", "c1ccc(cc1)-c2ccccc2"),
        ("isopropanol", "CC(C)O"),
        ("tert-butanol", "CC(C)(C)O"),
        ("dms", "CS(=O)C"),
        ("nitromethane", "C[N+](=O)[O-]"),
        ("phosphoric", "O=P(O)(O)O"),
        ("hexanoic", "CCCCCC(=O)O"),
        ("glucose", "OCC(O)C(O)C(O)C(O)C=O"),
        ("ethylhexanol", "CCCC(CC)CCO"),
        ("ibuprofen", "CC(C)Cc1ccc(cc1)C(C)C(=O)O"),
        ("paracetamol", "CC(=O)Nc1ccc(O)cc1"),
        ("nicotine", "CN1CCCC1c2cccnc2"),
    ];
    for (name, smiles) in cases {
        if only.as_deref().is_none_or(|want| want == *name) {
            chem(name, smiles, out);
        }
    }

    if only.is_none() || only.as_deref() == Some("mdl-ethanol") {
        let mol = "ethanol\n  test\n\n  3  2  0  0  0  0  0  0  0  0999 V2000\n    0.0000    0.0000    0.0000 C   0  0  0  0  0  0  0  0  0  0  0  0\n    1.0000    0.5000    0.0000 C   0  0  0  0  0  0  0  0  0  0  0  0\n    2.0000    0.0000    0.0000 O   0  0  0  0  0  0  0  0  0  0  0  0\n  1  2  1  0  0  0  0\n  2  3  1  0  0  0  0\nM  END\n";
        match plotter::input::mdl::mdl_to_scene(mol) {
            Ok(scene) => write("mdl-ethanol", scene.scene, out),
            Err(error) => println!("FAIL mdl-ethanol: {}", error.message()),
        }
    }

    if only.is_none() || only.as_deref() == Some("score-complex") {
        let score = "X:1\nT:Complex Stress\nC:Test\nQ:100\nM:6/8\nL:1/8\nK:A\nV:1 clef=bass\nC,2 (3C,D,E,) F,|\nB,,3 (3A,,B,,C,) D,|\n[E,G,]2 .[F,A,] [G,B,]|\n(C,D,E, F,G,A,)|\nw:la la la|";
        match plotter::input::abc::abc_to_scene(score) {
            Ok(scene) => write("score-complex", scene.scene, out),
            Err(error) => println!("FAIL score-complex: {}", error.message()),
        }
    }
    if only.is_none() || only.as_deref() == Some("score-cut") {
        let score = "X:1\nT:Cut March\nM:C|\nL:1/8\nK:D\nFGAB c2 B2|ABcd e2 d2|cBAG FGAB|DEFA d4|\n";
        match plotter::input::abc::abc_to_scene(score) {
            Ok(scene) => write("score-cut", scene.scene, out),
            Err(error) => println!("FAIL score-cut: {}", error.message()),
        }
    }
    println!("stress done");
}
