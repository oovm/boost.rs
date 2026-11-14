//! 与源语言无关的化学结构语义。
//!
//! [`MoleculeSpec`] 描述一个分子图：原子、键与可选的反应条件。它不携带任何位置、
//! 尺寸或绘制命令。SMILES / MDL 前端降级进这个模型，[`crate::depict`] 把它编译成
//! 平面描绘计划。

/// 元素。v0 覆盖常见有机与杂原子元素，其余保留 [`Element::Other`]。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Element {
    /// 氢。
    H,
    /// 硼。
    B,
    /// 碳。
    C,
    /// 氮。
    N,
    /// 氧。
    O,
    /// 氟。
    F,
    /// 硅。
    Si,
    /// 磷。
    P,
    /// 硫。
    S,
    /// 氯。
    Cl,
    /// 溴。
    Br,
    /// 碘。
    I,
    /// 表外元素，按符号原样标注且不做价校验。
    Other(String),
}

impl Element {
    /// 元素符号。
    pub fn symbol(&self) -> &str {
        match self {
            Self::H => "H",
            Self::B => "B",
            Self::C => "C",
            Self::N => "N",
            Self::O => "O",
            Self::F => "F",
            Self::Si => "Si",
            Self::P => "P",
            Self::S => "S",
            Self::Cl => "Cl",
            Self::Br => "Br",
            Self::I => "I",
            Self::Other(symbol) => symbol,
        }
    }

    /// 常见价的基值；表外元素返回 `None`，跳过价校验。
    pub fn base_valence(&self) -> Option<u8> {
        match self {
            Self::H | Self::F | Self::Cl | Self::Br | Self::I => Some(1),
            Self::B => Some(3),
            Self::C | Self::Si => Some(4),
            Self::N | Self::P => Some(3),
            Self::O => Some(2),
            Self::S => Some(2),
            Self::Other(_) => None,
        }
    }

    /// 允许的上限价：硫与磷有常见超价形态（亚砜、砜、磷酸酯），卤素在高价
    /// 含氧酸根里可到七。其余元素与基值一致。
    pub fn max_valence(&self) -> i16 {
        match self {
            Self::S => 6,
            Self::P => 5,
            Self::Cl | Self::Br | Self::I => 7,
            other => i16::from(other.base_valence().unwrap_or(0)),
        }
    }
}

/// 键级。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BondOrder {
    /// 单键。
    Single,
    /// 双键。
    Double,
    /// 三键。
    Triple,
    /// 芳香键。
    Aromatic,
}

/// 一个原子。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Atom {
    /// 元素。
    pub element: Element,
    /// 形式电荷。
    pub charge: i8,
    /// 同位素质量数。
    pub isotope: Option<u16>,
    /// 显式氢数；`None` 表示按价隐含。
    pub hydrogens: Option<u8>,
}

impl Atom {
    /// 创建一个中性、无同位素、隐式氢的原子。
    pub fn new(element: Element) -> Self {
        Self { element, charge: 0, isotope: None, hydrogens: None }
    }

    /// 是否需要绘制元素符号。碳骨架默认不标，带电荷或同位素时才标。
    pub fn labelled(&self) -> bool {
        !matches!(self.element, Element::C) || self.charge != 0 || self.isotope.is_some()
    }
}

/// 一条键。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bond {
    /// 起点原子下标。
    pub from: usize,
    /// 终点原子下标。
    pub to: usize,
    /// 键级。
    pub order: BondOrder,
}

impl Bond {
    /// 创建一条键。
    pub fn new(from: usize, to: usize, order: BondOrder) -> Self {
        Self { from, to, order }
    }
}

/// 一个分子。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoleculeSpec {
    /// 原子，下标即键端点引用。
    pub atoms: Vec<Atom>,
    /// 键。
    pub bonds: Vec<Bond>,
}

impl MoleculeSpec {
    /// 创建一个空分子。
    pub fn new() -> Self {
        Self { atoms: Vec::new(), bonds: Vec::new() }
    }

    /// 追加原子并返回下标。
    pub fn push_atom(&mut self, atom: Atom) -> usize {
        self.atoms.push(atom);
        self.atoms.len() - 1
    }

    /// 追加一条键。
    pub fn push_bond(&mut self, bond: Bond) {
        self.bonds.push(bond);
    }
}

impl Default for MoleculeSpec {
    fn default() -> Self {
        Self::new()
    }
}

/// 反应箭头。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrowKind {
    /// 单步反应。
    Single,
    /// 平衡（双向）。
    Equilibrium,
    /// 不可逆（双箭头）。
    Double,
}

/// 一个反应式。
#[derive(Clone, Debug, PartialEq)]
pub struct ReactionSpec {
    /// 反应物，从左到右。
    pub reactants: Vec<MoleculeSpec>,
    /// 产物，从左到右。
    pub products: Vec<MoleculeSpec>,
    /// 箭头上方条件（如催化剂）。
    pub above: Option<String>,
    /// 箭头下方条件（如温度、溶剂）。
    pub below: Option<String>,
    /// 箭头类型。
    pub arrow: ArrowKind,
}

impl ReactionSpec {
    /// 创建一个单步反应。
    pub fn new(reactants: Vec<MoleculeSpec>, products: Vec<MoleculeSpec>) -> Self {
        Self { reactants, products, above: None, below: None, arrow: ArrowKind::Single }
    }
}
