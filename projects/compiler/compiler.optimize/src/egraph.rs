//! 基于 hashcons 的 `E-Graph` 宿主：union-find、congruence rebuild 与饱和迭代。
//!
//! 常量折叠语义不硬编码任何语言操作命名空间，由消费者经 [`ConstantInterpreter`] 显式提交。

use crate::{
    Identifier, QualifiedName,
    extract::{ExtractCost, extract_class},
    pattern::{EGraphMatchView, Substitution, TermPattern, match_pattern},
    term::Term,
};
use std::collections::{HashMap, HashSet, VecDeque};

/// 等价类标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EClassId(usize);

/// `E-Node` 标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ENodeId(usize);

/// 规范化后的 `E-Node` 形状，用于 hashcons。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CanonicalENode {
    Literal(i64),
    Symbol(QualifiedName),
    Apply { operator: QualifiedName, children: Vec<usize> },
}

/// 实际存储的 `E-Node`。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ENode {
    Literal(i64),
    Symbol(QualifiedName),
    Apply { operator: QualifiedName, children: Vec<usize> },
}

/// 消费者提交的常量折叠解释，按操作限定名分派。
///
/// 底座不内置任何语言的操作命名空间；折叠的算术语义、饱和语义与溢出策略均由实现方决定。
pub trait ConstantInterpreter {
    /// 折叠二元应用；不是已知操作或无法折叠时返回 `None`。
    fn fold_binary(&self, operator: &QualifiedName, left: i64, right: i64) -> Option<i64>;

    /// 折叠一元应用；默认不折叠任何一元操作。
    fn fold_unary(&self, operator: &QualifiedName, operand: i64) -> Option<i64> {
        let _ = (operator, operand);
        None
    }
}

/// 一次 `E-Graph` 运行统计。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EGraphStats {
    /// 等价类数量。
    pub equivalence_class_count: usize,
    /// 被管理的 `E-Node` 数量。
    pub enode_count: usize,
    /// 被应用的 flat 等式数量。
    pub applied_equation_count: usize,
    /// 被应用的模式重写数量。
    pub applied_rewrite_count: usize,
    /// 常量折叠次数。
    pub constant_fold_count: usize,
    /// 饱和迭代轮次。
    pub iteration_count: usize,
    /// 是否达到当前理论下的不动点。
    pub saturated: bool,
}

/// 结构化重写条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredRewrite {
    /// 规则名。
    pub name: Identifier,
    /// 左侧模式。
    pub left: TermPattern,
    /// 右侧模式。
    pub right: TermPattern,
}

/// `E-Graph` 宿主。
#[derive(Debug, Default)]
pub struct EGraphHost {
    parents: Vec<usize>,
    ranks: Vec<u8>,
    class_enodes: Vec<HashSet<ENodeId>>,
    enodes: Vec<ENode>,
    enode_class: Vec<EClassId>,
    hashcons: HashMap<CanonicalENode, EClassId>,
    pending: VecDeque<(usize, usize)>,
    stats: EGraphStats,
}

impl EGraphHost {
    /// 插入一个结构化项并返回其等价类。
    pub fn add_term(&mut self, term: &Term) -> EClassId {
        match term {
            Term::Literal(value) => self.add_literal(*value),
            Term::Symbol(name) => self.add_symbol(name.clone()),
            Term::Apply { operator, arguments } => {
                let children = arguments.iter().map(|argument| self.add_term(argument).0).collect::<Vec<_>>();
                self.add_apply(operator.clone(), children)
            }
        }
    }

    /// 插入 flat 限定名，供 flat 等式使用。
    pub fn add_flat_symbol(&mut self, name: QualifiedName) -> EClassId {
        self.add_symbol(name)
    }

    /// 合并两个 flat 限定名；若产生新合并则返回 `true`。
    pub fn union_flat(&mut self, left: &QualifiedName, right: &QualifiedName) -> bool {
        let left_class = self.add_flat_symbol(left.clone());
        let right_class = self.add_flat_symbol(right.clone());
        self.union_classes(left_class, right_class)
    }

    /// 返回限定名对应的等价类（已合并到代表元）。
    pub fn class_of_symbol(&self, name: &QualifiedName) -> Option<EClassId> {
        self.hashcons.get(&CanonicalENode::Symbol(name.clone())).map(|class| EClassId(self.find_immut(class.0)))
    }

    /// 按阶段执行饱和并返回统计信息。
    ///
    /// `folding` 为 `None` 时不执行任何常量折叠。
    pub fn saturate(
        &mut self,
        rewrites: &[StructuredRewrite],
        flat_equations: &[(QualifiedName, QualifiedName)],
        folding: Option<&dyn ConstantInterpreter>,
    ) -> EGraphStats {
        for (left, right) in flat_equations {
            if self.union_flat(left, right) {
                self.stats.applied_equation_count += 1;
            }
        }

        let mut changed = true;
        while changed {
            changed = false;
            self.stats.iteration_count += 1;
            if self.rebuild_congruence() {
                changed = true;
            }
            if self.apply_rewrites(rewrites) {
                changed = true;
            }
            if self.apply_constant_folds(folding) {
                changed = true;
            }
        }

        self.stats.equivalence_class_count = self.count_classes();
        self.stats.enode_count = self.enodes.len();
        self.stats.saturated = !changed;
        self.stats.clone()
    }

    /// 从等价类抽取最优结构化项。
    pub fn extract_term(&self, class: EClassId) -> Term {
        extract_class(self, class.0)
    }

    /// 返回当前统计快照。
    pub fn stats(&self) -> &EGraphStats {
        &self.stats
    }

    fn add_literal(&mut self, value: i64) -> EClassId {
        self.lookup_or_insert(CanonicalENode::Literal(value), ENode::Literal(value))
    }

    fn add_symbol(&mut self, name: QualifiedName) -> EClassId {
        self.lookup_or_insert(CanonicalENode::Symbol(name.clone()), ENode::Symbol(name))
    }

    fn add_apply(&mut self, operator: QualifiedName, children: Vec<usize>) -> EClassId {
        let canonical_children = children.iter().map(|child| self.find_immut(*child)).collect::<Vec<_>>();
        let canonical = CanonicalENode::Apply { operator: operator.clone(), children: canonical_children.clone() };
        let enode = ENode::Apply { operator, children: canonical_children };
        self.lookup_or_insert(canonical, enode)
    }

    fn lookup_or_insert(&mut self, canonical: CanonicalENode, enode: ENode) -> EClassId {
        if let Some(existing) = self.hashcons.get(&canonical).copied() {
            return existing;
        }

        let class = EClassId(self.parents.len());
        let enode_id = ENodeId(self.enodes.len());
        self.parents.push(class.0);
        self.ranks.push(0);
        self.class_enodes.push(HashSet::from([enode_id]));
        self.enodes.push(enode);
        self.enode_class.push(class);
        self.hashcons.insert(canonical, class);
        class
    }

    fn union_classes(&mut self, left: EClassId, right: EClassId) -> bool {
        let left_root = self.find(left.0);
        let right_root = self.find(right.0);
        if left_root == right_root {
            return false;
        }

        self.pending.push_back((left_root, right_root));
        true
    }

    fn merge_classes(&mut self, keep: usize, merge: usize) {
        if keep == merge {
            return;
        }

        let merged_enodes = std::mem::take(&mut self.class_enodes[merge]);
        for enode in merged_enodes {
            self.enode_class[enode.0] = EClassId(keep);
            self.class_enodes[keep].insert(enode);
        }
        self.parents[merge] = keep;
    }

    fn rebuild_congruence(&mut self) -> bool {
        let mut changed = false;
        while let Some((left, right)) = self.pending.pop_front() {
            let left_root = self.find(left);
            let right_root = self.find(right);
            if left_root == right_root {
                continue;
            }

            let left_enodes = self.class_enodes[left_root].iter().copied().collect::<Vec<_>>();
            let right_enodes = self.class_enodes[right_root].iter().copied().collect::<Vec<_>>();
            for left_enode in &left_enodes {
                for right_enode in &right_enodes {
                    if let Some((left_class, right_class)) = congruent_enode_classes(self, left_enode.0, right_enode.0) {
                        if self.union_classes(left_class, right_class) {
                            changed = true;
                        }
                    }
                }
            }

            self.merge_classes(left_root, right_root);
            changed = true;
        }
        changed
    }

    fn apply_rewrites(&mut self, rewrites: &[StructuredRewrite]) -> bool {
        let mut changed = false;
        let class_roots = (0..self.parents.len()).map(|index| self.find(index)).collect::<HashSet<_>>();
        for class_root in class_roots {
            let class = EClassId(class_root);
            for rewrite in rewrites {
                let Some(substitution) = match_pattern(self, &rewrite.left, class.0)
                else {
                    continue;
                };
                let target = self.instantiate_pattern(&substitution, &rewrite.right);
                if self.union_classes(class, target) {
                    self.stats.applied_rewrite_count += 1;
                    changed = true;
                }
            }
        }
        changed
    }

    fn instantiate_pattern(&mut self, substitution: &Substitution, pattern: &TermPattern) -> EClassId {
        match pattern {
            TermPattern::Variable(name) => {
                if let Some(class) = substitution.class_for(name) {
                    EClassId(self.find(class))
                }
                else if let Some(value) = substitution.literal_for(name) {
                    self.add_literal(value)
                }
                else {
                    self.add_symbol(QualifiedName::new(vec![name.clone()]))
                }
            }
            TermPattern::Literal(value) => self.add_literal(*value),
            TermPattern::Symbol(name) => self.add_symbol(name.clone()),
            TermPattern::Apply { operator, arguments } => {
                let children = arguments.iter().map(|argument| self.instantiate_pattern(substitution, argument).0).collect();
                self.add_apply(operator.clone(), children)
            }
        }
    }

    fn apply_constant_folds(&mut self, folding: Option<&dyn ConstantInterpreter>) -> bool {
        let Some(folding) = folding
        else {
            return false;
        };
        let mut changed = false;
        let enode_count = self.enodes.len();
        for enode_index in 0..enode_count {
            let Some((operator, children)) = self.enode_apply(enode_index)
            else {
                continue;
            };
            let folded = match children.len() {
                1 => literal_in_class(self, children[0]).and_then(|operand| folding.fold_unary(operator, operand)),
                2 => match (literal_in_class(self, children[0]), literal_in_class(self, children[1])) {
                    (Some(left), Some(right)) => folding.fold_binary(operator, left, right),
                    _ => None,
                },
                _ => None,
            };
            let Some(result) = folded
            else {
                continue;
            };
            let literal_class = self.add_literal(result);
            let enode_class = self.enode_class[enode_index];
            if self.union_classes(enode_class, literal_class) {
                self.stats.constant_fold_count += 1;
                changed = true;
            }
        }
        changed
    }

    fn count_classes(&mut self) -> usize {
        let mut roots = HashSet::new();
        for index in 0..self.parents.len() {
            roots.insert(self.find(index));
        }
        roots.len().max(1)
    }

    fn find(&mut self, index: usize) -> usize {
        let parent = self.parents[index];
        if parent == index {
            index
        }
        else {
            let root = self.find(parent);
            self.parents[index] = root;
            root
        }
    }

    fn find_immut(&self, index: usize) -> usize {
        let mut current = index;
        while self.parents[current] != current {
            current = self.parents[current];
        }
        current
    }

    pub(crate) fn enode_to_term_with(&self, enode: usize, extract_child: &impl Fn(usize) -> Term) -> Term {
        let children = self.enode_children(enode);
        let arguments = children.into_iter().map(extract_child).collect();
        self.enode_to_term_from_children(enode, arguments)
    }

    pub(crate) fn enode_to_term_from_children(&self, enode: usize, arguments: Vec<Term>) -> Term {
        match &self.enodes[enode] {
            ENode::Literal(value) => Term::Literal(*value),
            ENode::Symbol(name) => Term::Symbol(name.clone()),
            ENode::Apply { operator, .. } => Term::Apply { operator: operator.clone(), arguments },
        }
    }
}

impl EGraphMatchView for EGraphHost {
    fn find(&self, class: usize) -> usize {
        self.find_immut(class)
    }

    fn enodes_in_class(&self, class: usize) -> Vec<usize> {
        let root = self.find_immut(class);
        self.class_enodes.get(root).map(|enodes| enodes.iter().map(|enode| enode.0).collect()).unwrap_or_default()
    }

    fn enode_literal(&self, enode: usize) -> Option<i64> {
        match self.enodes.get(enode)? {
            ENode::Literal(value) => Some(*value),
            _ => None,
        }
    }

    fn enode_symbol(&self, enode: usize) -> Option<&QualifiedName> {
        match self.enodes.get(enode)? {
            ENode::Symbol(name) => Some(name),
            _ => None,
        }
    }

    fn enode_apply(&self, enode: usize) -> Option<(&QualifiedName, &[usize])> {
        match self.enodes.get(enode)? {
            ENode::Apply { operator, children } => Some((operator, children.as_slice())),
            ENode::Literal(_) | ENode::Symbol(_) => None,
        }
    }
}

impl ExtractCost for EGraphHost {
    fn find_class(&self, class: usize) -> usize {
        self.find_immut(class)
    }

    fn enodes_in_class(&self, class: usize) -> Vec<usize> {
        EGraphMatchView::enodes_in_class(self, class)
    }

    fn enode_kind_cost(&self, enode: usize) -> u32 {
        match &self.enodes[enode] {
            ENode::Literal(_) => 1,
            ENode::Symbol(_) => 4,
            ENode::Apply { operator, children } => 2 + operator.parts().len() as u32 + children.len() as u32,
        }
    }

    fn enode_children(&self, enode: usize) -> Vec<usize> {
        match &self.enodes[enode] {
            ENode::Literal(_) | ENode::Symbol(_) => Vec::new(),
            ENode::Apply { children, .. } => children.clone(),
        }
    }

    fn enode_to_term(&self, enode: usize) -> Term {
        self.enode_to_term_with(enode, &|child| extract_class(self, child))
    }
}

fn congruent_enode_classes(graph: &EGraphHost, left: usize, right: usize) -> Option<(EClassId, EClassId)> {
    let (left_enode, right_enode) = (&graph.enodes[left], &graph.enodes[right]);
    match (left_enode, right_enode) {
        (ENode::Apply { operator: left_op, children: left_children }, ENode::Apply { operator: right_op, children: right_children })
            if left_op == right_op && left_children.len() == right_children.len() =>
        {
            let all_congruent = left_children
                .iter()
                .zip(right_children.iter())
                .all(|(left_child, right_child)| graph.find_immut(*left_child) == graph.find_immut(*right_child));
            if all_congruent {
                return Some((graph.enode_class[left], graph.enode_class[right]));
            }
            None
        }
        _ => None,
    }
}

fn literal_in_class(graph: &EGraphHost, class: usize) -> Option<i64> {
    let root = graph.find_immut(class);
    graph.class_enodes.get(root)?.iter().find_map(|enode| graph.enode_literal(enode.0))
}
