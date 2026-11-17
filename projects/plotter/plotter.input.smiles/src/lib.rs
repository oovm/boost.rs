#![deny(missing_docs)]
#![doc = include_str!("../readme.md")]

//! SMILES 前端：`oak-smiles` 的 AST 降级进 `plotter.depiction` 的与源语言无关模型。
//!
//! 文本语法与原子/键的遍历顺序都由 Oak 负责，本 crate 只做语义映射。立体信息与
//! `~` 任意键在 v0 描绘里没有表示，映射时保留可画的部分并记录
//! `davinci.depiction.approximated` 命名损失；结构本身无法描绘（没有原子、反应式
//! 缺反应物或产物）时才整例拒绝。

use std::ops::Range;

use oak_core::{Builder, SourceText, errors::OakError, parser::session::ParseSession};
use oak_smiles::{
    SmilesBuilder, SmilesLanguage,
    ast::{SmilesAtom, SmilesBond, SmilesBondOrder, SmilesItem, SmilesRoot},
};
use plotter_depiction::{
    DepictionError, DepictionOptions, LoweredDepiction,
    spec::{ArrowKind, Atom, Bond, BondOrder, Element, MoleculeSpec, ReactionSpec},
};
use plotter_types::{
    Diagnostic, DiagnosticLabel, DiagnosticLocation, DiagnosticSeverity, LabelRole, Message, NAMESPACE, SourceRef, codes, diagnostic,
};

/// 诊断与标签里使用的源身份。
const SOURCE: &str = "smiles";

/// SMILES 前端降级结果：分子或反应式，加前端记录的命名损失。
#[derive(Clone, Debug, PartialEq)]
pub enum StructureSpec {
    /// 一个分子（可能含断开片段）。
    Molecule(MoleculeSpec),
    /// 一个反应式。
    Reaction(ReactionSpec),
}

/// SMILES 前端的降级结果。
#[derive(Clone, Debug, PartialEq)]
pub struct LoweredStructure {
    /// 可交给 `plotter.depiction` 描绘的结构。
    pub spec: StructureSpec,
    /// 前端在映射中记录的非致命损失。
    pub diagnostics: Vec<Diagnostic>,
}

/// 解析 SMILES 源码，返回保留字节跨度的 Oak AST。
pub fn parse_smiles_source(source: &str) -> Result<SmilesRoot, DepictionError> {
    let text = SourceText::new(source);
    let builder = SmilesBuilder;
    let mut session = ParseSession::<SmilesLanguage>::default();
    let output = builder.build(&text, &[], &mut session);
    match output.result {
        Ok(root) => Ok(root),
        Err(error) => Err(spanned_parse_error(source, &error)),
    }
}

/// 把 SMILES 源码降级成分子或反应式，不描绘。
pub fn smiles_to_spec(source: &str) -> Result<LoweredStructure, DepictionError> {
    let root = parse_smiles_source(source)?;
    lower_root(source, &root)
}

/// 把 SMILES 源码直接描绘成显示场景。
pub fn smiles_to_scene(source: &str) -> Result<LoweredDepiction, DepictionError> {
    let lowered = smiles_to_spec(source)?;
    let mut depicted = match &lowered.spec {
        StructureSpec::Molecule(molecule) => plotter_depiction::molecule_to_scene_with_options(molecule, &DepictionOptions::default())?,
        StructureSpec::Reaction(reaction) => plotter_depiction::reaction_to_scene_with_options(reaction, &DepictionOptions::default())?,
    };
    depicted.plan.diagnostics.extend(lowered.diagnostics);
    depicted.diagnostics = depicted.plan.diagnostics.clone();
    Ok(depicted)
}

/// 把解析错误转成带跨度的拒绝。
fn spanned_parse_error(source: &str, error: &OakError) -> DepictionError {
    if let oak_core::errors::OakErrorKind::SyntaxError { message, offset, .. } = error.kind() {
        let offset = (*offset).min(source.len());
        let end = source[offset..].find('\n').map(|delta| offset + delta).unwrap_or(source.len());
        return DepictionError::unsupported(message.clone()).with_span(offset..end);
    }
    DepictionError::unsupported(format!("`oak-smiles` failed to build an AST: {error}"))
}

/// 把源字节跨度转成诊断跨度。
fn span_of(range: &core::range::Range<usize>) -> Range<usize> {
    range.start..range.end
}

/// 记录一条带字节跨度的命名损失。
fn approximated(losses: &mut Vec<Diagnostic>, message: impl Into<String>, span: Option<Range<usize>>) {
    let message = message.into();
    let mut record = diagnostic("depiction", codes::DEPICTION_APPROXIMATED, DiagnosticSeverity::Warning, message.clone());
    let Some(span) = span.filter(|span| span.start < span.end)
    else {
        losses.push(record);
        return;
    };
    let Ok(source) = SourceRef::new(NAMESPACE, SOURCE)
    else {
        losses.push(record);
        return;
    };
    let Ok(range) = plotter_types::ByteRange::new(span.start as u64, span.end as u64)
    else {
        losses.push(record);
        return;
    };
    record = record.with_primary(DiagnosticLabel::new(
        DiagnosticLocation::Text { source, range },
        Message::new(codes::DEPICTION_APPROXIMATED).with_fallback(message),
        LabelRole::Primary,
    ));
    losses.push(record);
}

/// 降级一个 SMILES 文档。
fn lower_root(source: &str, root: &SmilesRoot) -> Result<LoweredStructure, DepictionError> {
    let mut diagnostics = Vec::new();
    if root.arrow.is_some() || !root.products.is_empty() {
        let reaction = lower_reaction(source, root, &mut diagnostics)?;
        return Ok(LoweredStructure { spec: StructureSpec::Reaction(reaction), diagnostics });
    }
    if root.items.is_empty() {
        return Err(DepictionError::unsupported("the SMILES source declares no atom to depict"));
    }
    let molecule = merge_items(&root.items, &mut diagnostics);
    Ok(LoweredStructure { spec: StructureSpec::Molecule(molecule), diagnostics })
}

/// 降级一个反应式。
fn lower_reaction(_source: &str, root: &SmilesRoot, diagnostics: &mut Vec<Diagnostic>) -> Result<ReactionSpec, DepictionError> {
    let arrow = root.arrow.as_ref();
    let reactants: Vec<MoleculeSpec> = root.items.iter().map(|item| merge_items(std::slice::from_ref(item), diagnostics)).collect();
    let products: Vec<MoleculeSpec> = root.products.iter().map(|item| merge_items(std::slice::from_ref(item), diagnostics)).collect();
    if reactants.is_empty() || products.is_empty() {
        return Err(DepictionError::unsupported("a reaction needs at least one reactant and one product fragment on each side of the arrow"));
    }
    let kind = match arrow.map(|arrow| arrow.kind) {
        Some(oak_smiles::ast::SmilesArrowKind::Double) => ArrowKind::Double,
        _ => ArrowKind::Single,
    };
    let above = arrow.and_then(|arrow| arrow.above.clone());
    if above.is_none() && !root.reagents.is_empty() {
        diagnostics.push(diagnostic(
            "depiction",
            codes::DEPICTION_APPROXIMATED,
            DiagnosticSeverity::Warning,
            "reagent fragments are drawn as plain arrow text without their structures",
        ));
    }
    Ok(ReactionSpec { reactants, products, above, below: arrow.and_then(|arrow| arrow.below.clone()), arrow: kind })
}

/// 把若干片段合并成一个分子图：原子下标按片段顺序平移。
fn merge_items(items: &[SmilesItem], diagnostics: &mut Vec<Diagnostic>) -> MoleculeSpec {
    let mut molecule = MoleculeSpec::new();
    for item in items {
        let offset = molecule.atoms.len();
        for atom in &item.atoms {
            molecule.push_atom(lower_atom(atom, diagnostics));
        }
        for bond in &item.bonds {
            molecule.push_bond(lower_bond(bond, offset, diagnostics));
        }
    }
    molecule
}

/// 原子映射。
fn lower_atom(atom: &SmilesAtom, diagnostics: &mut Vec<Diagnostic>) -> Atom {
    if atom.element == "*" {
        approximated(diagnostics, "an author-written wildcard atom is drawn as `*` with no valence check", Some(span_of(&atom.span)));
    }
    if atom.chirality.is_some() {
        approximated(diagnostics, "tetrahedral chirality (`@` / `@@`) has no v0 depiction and is not drawn", Some(span_of(&atom.span)));
    }
    Atom { element: element_from_symbol(&atom.element), charge: atom.charge, isotope: atom.isotope, hydrogens: atom.explicit_hydrogens }
}

/// 键映射。
fn lower_bond(bond: &SmilesBond, offset: usize, diagnostics: &mut Vec<Diagnostic>) -> Bond {
    if bond.stereo.is_some() {
        approximated(diagnostics, "double-bond direction (`/` or `\\`) has no v0 depiction and is not drawn", Some(span_of(&bond.span)));
    }
    let order = match bond.order {
        SmilesBondOrder::Single => BondOrder::Single,
        SmilesBondOrder::Double => BondOrder::Double,
        SmilesBondOrder::Triple => BondOrder::Triple,
        SmilesBondOrder::Aromatic => BondOrder::Aromatic,
        SmilesBondOrder::Any => {
            approximated(diagnostics, "a `~` any-bond is drawn as a single bond", Some(span_of(&bond.span)));
            BondOrder::Single
        }
    };
    Bond::new(bond.from + offset, bond.to + offset, order)
}

/// 元素符号映射：已知有机与杂原子元素走枚举，其余按符号原样保留。
fn element_from_symbol(symbol: &str) -> Element {
    let mut chars = symbol.chars();
    let canonical = match chars.next() {
        None => String::new(),
        Some(first) => {
            let mut text = first.to_ascii_uppercase().to_string();
            text.push_str(&chars.as_str().to_ascii_lowercase());
            text
        }
    };
    match canonical.as_str() {
        "H" => Element::H,
        "B" => Element::B,
        "C" => Element::C,
        "N" => Element::N,
        "O" => Element::O,
        "F" => Element::F,
        "Si" => Element::Si,
        "P" => Element::P,
        "S" => Element::S,
        "Cl" => Element::Cl,
        "Br" => Element::Br,
        "I" => Element::I,
        other => Element::Other(other.to_string()),
    }
}
