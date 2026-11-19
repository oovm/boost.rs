#![warn(missing_docs)]
#![doc = include_str!("readme.md")]

//! MDL 前端：`data-text-mdl` 的 AST 降级进 `plotter.depiction` 的与源语言无关模型。
//!
//! 连接表语法与 1 基原子下标由 `data-text-mdl` 保证；本 crate 负责把连接表压成
//! [`MoleculeSpec`]：原子行、键行、`M  CHG` / `M  ISO` 属性合并成单一原子图。
//! 坐标、质量差、立体奇偶、放射标记等 v0 描绘没有表示的字段保留可画部分并记录
//! `davinci.depiction.approximated`；原子或键越界才整例拒绝。

use std::ops::Range;

use data_text_core::{Builder, SourceText, errors::TextError, parser::session::ParseSession};
use data_text_mdl::{
    MdlBuilder, MdlLanguage,
    ast::{MdlBond, MdlBondOrder, MdlRecord, MdlRoot},
};
use plotter_depiction::{
    DepictionError, DepictionOptions, LoweredDepiction,
    spec::{Atom, Bond, BondOrder, Element, MoleculeSpec},
};
use plotter_types::{
    Diagnostic, DiagnosticLabel, DiagnosticLocation, DiagnosticSeverity, LabelRole, Message, NAMESPACE, SourceRef, codes, diagnostic,
};

/// 诊断与标签里使用的源身份。
const SOURCE: &str = "mdl";

/// MDL 前端降级结果：一个分子图加前端记录的命名损失。
#[derive(Clone, Debug, PartialEq)]
pub struct LoweredStructure {
    /// 可交给 `plotter.depiction` 描绘的分子。
    pub spec: MoleculeSpec,
    /// 前端在映射中记录的非致命损失。
    pub diagnostics: Vec<Diagnostic>,
}

/// 解析 MDL 源码，返回保留字节跨度的语法树。
pub fn parse_mdl_source(source: &str) -> Result<MdlRoot, DepictionError> {
    let text = SourceText::new(source);
    let builder = MdlBuilder;
    let mut session = ParseSession::<MdlLanguage>::default();
    let output = builder.build(&text, &[], &mut session);
    match output.result {
        Ok(root) => Ok(root),
        Err(error) => Err(spanned_parse_error(source, &error)),
    }
}

/// 把 MDL 源码降级成分子图，不描绘。
pub fn mdl_to_spec(source: &str) -> Result<LoweredStructure, DepictionError> {
    let root = parse_mdl_source(source)?;
    let mut diagnostics = Vec::new();
    if root.records.len() > 1 {
        approximated(
            &mut diagnostics,
            format!("the source declares {} SDF records; only the first one is depicted", root.records.len()),
            root.records.get(1).map(|record| span_of(&record.span)),
        );
    }
    let record = root.records.first().ok_or_else(|| DepictionError::unsupported("the MDL source declares no record to depict"))?;
    let spec = lower_record(record, &mut diagnostics)?;
    Ok(LoweredStructure { spec, diagnostics })
}

/// 把 MDL 源码直接描绘成显示场景。
pub fn mdl_to_scene(source: &str) -> Result<LoweredDepiction, DepictionError> {
    let lowered = mdl_to_spec(source)?;
    let mut depicted = plotter_depiction::molecule_to_scene_with_options(&lowered.spec, &DepictionOptions::default())?;
    depicted.plan.diagnostics.extend(lowered.diagnostics);
    depicted.diagnostics = depicted.plan.diagnostics.clone();
    Ok(depicted)
}

/// 把解析错误转成带跨度的拒绝。
fn spanned_parse_error(source: &str, error: &TextError) -> DepictionError {
    if let data_text_core::errors::TextErrorKind::SyntaxError { message, offset, .. } = error.kind() {
        let offset = (*offset).min(source.len());
        let end = source[offset..].find('\n').map(|delta| offset + delta).unwrap_or(source.len());
        return DepictionError::unsupported(message.clone()).with_span(offset..end);
    }
    DepictionError::unsupported(format!("`data-text-mdl` failed to build an AST: {error}"))
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

/// 降级一条 MDL 记录。
fn lower_record(record: &MdlRecord, diagnostics: &mut Vec<Diagnostic>) -> Result<MoleculeSpec, DepictionError> {
    approximated(diagnostics, "authored 2D/3D coordinates are replaced by the deterministic depiction layout", Some(span_of(&record.span)));
    let mut molecule = MoleculeSpec::new();
    for atom in &record.atoms {
        if atom.mass_difference != 0 {
            approximated(
                diagnostics,
                format!("atom `{}` has a mass difference; the relative mass has no absolute isotope mapping", atom.element),
                Some(span_of(&atom.span)),
            );
        }
        if atom.z.abs() > f64::EPSILON {
            approximated(diagnostics, "a non-planar atom is flattened onto the depiction plane", Some(span_of(&atom.span)));
        }
        if atom.stereo != 0 {
            approximated(diagnostics, "atom stereo parity is not depicted", Some(span_of(&atom.span)));
        }
        molecule.push_atom(Atom { element: element_from_symbol(&atom.element), charge: atom.charge, isotope: None, hydrogens: None });
    }
    for bond in &record.bonds {
        molecule.push_bond(lower_bond(bond, record, diagnostics)?);
    }
    apply_properties(record, &mut molecule, diagnostics);
    Ok(molecule)
}

/// 键映射：1 基端点转 0 基，越界拒绝。
fn lower_bond(bond: &MdlBond, record: &MdlRecord, diagnostics: &mut Vec<Diagnostic>) -> Result<Bond, DepictionError> {
    let count = record.atoms.len();
    if bond.from == 0 || bond.to == 0 || bond.from > count || bond.to > count {
        return Err(DepictionError::unsupported(format!("bond endpoint ({}, {}) is outside the {count} declared atoms", bond.from, bond.to))
            .with_span(span_of(&bond.span)));
    }
    if bond.stereo != 0 {
        approximated(diagnostics, "bond stereo parity is not depicted", Some(span_of(&bond.span)));
    }
    let order = match bond.order {
        MdlBondOrder::Single => BondOrder::Single,
        MdlBondOrder::Double => BondOrder::Double,
        MdlBondOrder::Triple => BondOrder::Triple,
        MdlBondOrder::Aromatic => BondOrder::Aromatic,
        MdlBondOrder::Other(order) => {
            approximated(
                diagnostics,
                format!("bond order {order} is outside the v0 subset and is drawn as a single bond"),
                Some(span_of(&bond.span)),
            );
            BondOrder::Single
        }
    };
    Ok(Bond::new(bond.from - 1, bond.to - 1, order))
}

/// 应用 `M  CHG` / `M  ISO`，其余属性块记命名损失。
fn apply_properties(record: &MdlRecord, molecule: &mut MoleculeSpec, diagnostics: &mut Vec<Diagnostic>) {
    for property in &record.properties {
        match property.kind.trim() {
            "CHG" => {
                for entry in &property.entries {
                    if let Some(atom) = entry.atom.checked_sub(1).and_then(|index| molecule.atoms.get_mut(index)) {
                        atom.charge = entry.value.clamp(i16::from(i8::MIN), i16::from(i8::MAX)) as i8;
                    }
                }
            }
            "ISO" => {
                for entry in &property.entries {
                    if entry.value <= 0 {
                        continue;
                    }
                    if let Some(atom) = entry.atom.checked_sub(1).and_then(|index| molecule.atoms.get_mut(index)) {
                        atom.isotope = u16::try_from(entry.value).ok();
                    }
                }
            }
            kind => approximated(diagnostics, format!("property block `M  {kind}` is not depicted"), Some(span_of(&property.span))),
        }
    }
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
