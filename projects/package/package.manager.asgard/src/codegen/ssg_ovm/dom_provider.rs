//! `asgard.render` 宿主合同的 DOM provider 实现（从 VM 内核移入正式消费者）。
//!
//! DOM 节点是 GC 堆中的布局对象；布局身份、槽位序与读出器唯一归 Asgard 交付层
//! 所有，序列化语义（HTML escape、void 元素、作用域前缀）也在本 crate。
//! VM 只保留通用 provider、句柄与堆接口——宿主必须显式注册本 provider，
//! 未注册时含 DOM 导入的产物在加载期失败闭合。
//! 句柄即 `Value::Object`——类型化的堆引用，不是整数编号。

use interpreter_vm_ovm::{
    ObjectHeap, ObjectPayload, ObjectId, Value, array_runtime,
    error::OdinRuntimeError,
    host::{HostImport, HostProvider},
};

/// `HostElement` 的宿主私有布局身份。模块布局表由编译器从 0 起顺序分配，
/// 不会占用该值；读出器以此校验对象身份，不按槽位猜类型。
/// 该 id 只作身份标记：DOM 对象直接以 `heap.alloc` 创建，
/// 不经过 `alloc_layout_object`/`register_layout`（后者会按 id 扩容描述表）。
/// GC 追踪对 `LayoutObject` 全覆盖所有槽位，不依赖描述表。
pub(crate) const DOM_NODE_LAYOUT_ID: u32 = u32::MAX;

/// 节点种类：元素节点。
const DOM_KIND_ELEMENT: i32 = 1;
/// 节点种类：文本节点。
const DOM_KIND_TEXT: i32 = 2;

const SLOT_KIND: usize = 0;
const SLOT_TAG: usize = 1;
const SLOT_ATTRS: usize = 2;
const SLOT_CHILDREN: usize = 3;
const SLOT_TEXT: usize = 4;

/// 读出器递归上限：与 SSG 准入门的嵌套上限同量级，超出一律结构化失败。
const MAX_READ_TREE_DEPTH: usize = 256;

/// 从堆还原的显式类型化 DOM 树（SSG 读出 ABI 的产物形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DomNode {
    /// 元素节点：标签、属性键值（插入序）、子节点（插入序）与文本内容。
    Element {
        /// 宿主标签名。
        tag: String,
        /// 属性键值对，保持设置顺序；同名属性以最后一次写入为准。
        attrs: Vec<(String, String)>,
        /// 子节点，保持追加顺序。
        children: Vec<DomNode>,
        /// `dom_set_text` 写入的文本内容（写入会清空既有子节点）。
        text: String,
    },
    /// 文本节点。
    Text(String),
}

fn invalid_handle(actual: String) -> OdinRuntimeError {
    OdinRuntimeError::TypeMismatch { expected: "asgard.render HostElement", actual }
}

fn read_string(value: &Value, what: &'static str) -> Result<String, OdinRuntimeError> {
    match value {
        Value::String(text) => Ok(text.clone()),
        other => Err(OdinRuntimeError::TypeMismatch { expected: what, actual: other.type_name().to_string() }),
    }
}

/// 校验值为 DOM 节点并返回 (kind, ObjectId)。
fn dom_node_parts(heap: &ObjectHeap, value: &Value) -> Result<(i32, ObjectId), OdinRuntimeError> {
    let Value::Object(object_id) = value
    else {
        return Err(invalid_handle(value.type_name().to_string()));
    };
    match heap.get(*object_id) {
        Some(ObjectPayload::LayoutObject { layout_id, slots }) if *layout_id == DOM_NODE_LAYOUT_ID => match slots.get(SLOT_KIND) {
            Some(Value::I32(kind)) => Ok((*kind, *object_id)),
            _ => Err(OdinRuntimeError::ModuleLoad("DOM node missing kind slot".into())),
        },
        Some(_) => Err(invalid_handle("non-DOM object".into())),
        None => Err(OdinRuntimeError::ModuleLoad(format!("invalid object id {object_id}"))),
    }
}

fn require_element(heap: &ObjectHeap, value: &Value) -> Result<ObjectId, OdinRuntimeError> {
    let (kind, object_id) = dom_node_parts(heap, value)?;
    if kind != DOM_KIND_ELEMENT {
        return Err(OdinRuntimeError::TypeMismatch { expected: "element HostElement", actual: format!("DOM node kind {kind}") });
    }
    Ok(object_id)
}

fn set_slot(heap: &mut ObjectHeap, object_id: ObjectId, slot: usize, value: Value) -> Result<(), OdinRuntimeError> {
    heap.set_field(object_id, slot, value).map_err(|reason| match reason {
        "field slot out of range" => OdinRuntimeError::FieldSlotOutOfRange(slot as i32),
        "object not found" => OdinRuntimeError::ModuleLoad(format!("invalid object id {object_id}")),
        other => OdinRuntimeError::TypeMismatch { expected: "DOM node", actual: other.to_string() },
    })
}

fn fresh_node(heap: &mut ObjectHeap, kind: i32, tag: &str, text: &str) -> Result<Value, OdinRuntimeError> {
    let attrs = array_runtime::build_fixed_array(heap, 0, &[]);
    let children = array_runtime::build_fixed_array(heap, 0, &[]);
    Ok(Value::Object(heap.alloc(ObjectPayload::LayoutObject {
        layout_id: DOM_NODE_LAYOUT_ID,
        slots: vec![Value::I32(kind), Value::String(tag.to_string()), attrs, children, Value::String(text.to_string())],
    })))
}

/// `dom_create_element(tag)`：创建元素节点。空标签名结构化拒绝。
pub(crate) fn create_element(heap: &mut ObjectHeap, tag: &str) -> Result<Value, OdinRuntimeError> {
    if tag.is_empty() {
        return Err(OdinRuntimeError::TypeMismatch { expected: "non-empty element tag", actual: "empty string".into() });
    }
    fresh_node(heap, DOM_KIND_ELEMENT, tag, "")
}

/// `dom_create_text(text)`：创建文本节点。
pub(crate) fn create_text(heap: &mut ObjectHeap, text: &str) -> Result<Value, OdinRuntimeError> {
    fresh_node(heap, DOM_KIND_TEXT, "", text)
}

/// `dom_append(parent, child)`：向元素末尾追加子节点，保持插入顺序。
/// 父节点必须是元素节点，子节点必须是 DOM 节点；违规结构化失败。
pub(crate) fn append(heap: &mut ObjectHeap, parent: &Value, child: &Value) -> Result<Value, OdinRuntimeError> {
    let parent_id = require_element(heap, parent)?;
    dom_node_parts(heap, child)?;
    let children = read_slot(heap, parent_id, SLOT_CHILDREN)?;
    let Value::Object(children_id) = children
    else {
        return Err(OdinRuntimeError::ModuleLoad("DOM node children slot is not an object".into()));
    };
    heap.append_field(children_id, child.clone()).map_err(|reason| match reason {
        "object not found" => OdinRuntimeError::ModuleLoad(format!("invalid object id {children_id}")),
        other => OdinRuntimeError::TypeMismatch { expected: "layout array", actual: other.to_string() },
    })?;
    Ok(Value::Null)
}

/// `dom_set_attr(node, name, value)`：设置元素属性；同名覆盖，否则追加。
/// 空属性名结构化拒绝。
pub(crate) fn set_attr(heap: &mut ObjectHeap, node: &Value, name: &str, value: &str) -> Result<Value, OdinRuntimeError> {
    if name.is_empty() {
        return Err(OdinRuntimeError::TypeMismatch { expected: "non-empty attribute name", actual: "empty string".into() });
    }
    let node_id = require_element(heap, node)?;
    let attrs = read_slot(heap, node_id, SLOT_ATTRS)?;
    let Value::Object(attrs_id) = attrs
    else {
        return Err(OdinRuntimeError::ModuleLoad("DOM node attrs slot is not an object".into()));
    };
    let len = match heap.get(attrs_id) {
        Some(ObjectPayload::LayoutObject { slots, .. }) => slots.len(),
        _ => return Err(OdinRuntimeError::ModuleLoad(format!("invalid object id {attrs_id}"))),
    };
    for index in 0..len {
        let pair = array_runtime::array_get(heap, &attrs, &Value::I32(index as i32))?;
        let existing = array_runtime::array_get(heap, &pair, &Value::I32(0))?;
        if existing == Value::String(name.to_string()) {
            if let Value::Object(pair_id) = pair {
                set_slot(heap, pair_id, 1, Value::String(value.to_string()))?;
            }
            return Ok(Value::Null);
        }
    }
    let pair = array_runtime::build_fixed_array(heap, 0, &[Value::String(name.to_string()), Value::String(value.to_string())]);
    heap.append_field(attrs_id, pair).map_err(|reason| match reason {
        "object not found" => OdinRuntimeError::ModuleLoad(format!("invalid object id {attrs_id}")),
        other => OdinRuntimeError::TypeMismatch { expected: "layout array", actual: other.to_string() },
    })?;
    Ok(Value::Null)
}

/// `dom_get_attr(node, name)`：读取元素属性；未设置返回空串。
pub(crate) fn get_attr(heap: &ObjectHeap, node: &Value, name: &str) -> Result<Value, OdinRuntimeError> {
    let node_id = require_element(heap, node)?;
    let attrs = read_slot(heap, node_id, SLOT_ATTRS)?;
    let len = match &attrs {
        Value::Object(attrs_id) => match heap.get(*attrs_id) {
            Some(ObjectPayload::LayoutObject { slots, .. }) => slots.len(),
            _ => return Err(OdinRuntimeError::ModuleLoad(format!("invalid object id {attrs_id}"))),
        },
        _ => return Err(OdinRuntimeError::ModuleLoad("DOM node attrs slot is not an object".into())),
    };
    for index in 0..len {
        let pair = array_runtime::array_get(heap, &attrs, &Value::I32(index as i32))?;
        let existing = array_runtime::array_get(heap, &pair, &Value::I32(0))?;
        if existing == Value::String(name.to_string()) {
            return array_runtime::array_get(heap, &pair, &Value::I32(1));
        }
    }
    Ok(Value::String(String::new()))
}

/// `dom_set_text(node, text)`：设置元素文本内容并清空既有子节点。
pub(crate) fn set_text(heap: &mut ObjectHeap, node: &Value, text: &str) -> Result<Value, OdinRuntimeError> {
    let node_id = require_element(heap, node)?;
    let empty = array_runtime::build_fixed_array(heap, 0, &[]);
    set_slot(heap, node_id, SLOT_CHILDREN, empty)?;
    set_slot(heap, node_id, SLOT_TEXT, Value::String(text.to_string()))?;
    Ok(Value::Null)
}

fn read_slot(heap: &ObjectHeap, object_id: ObjectId, slot: usize) -> Result<Value, OdinRuntimeError> {
    match heap.get(object_id) {
        Some(ObjectPayload::LayoutObject { slots, .. }) => slots.get(slot).cloned().ok_or(OdinRuntimeError::FieldSlotOutOfRange(slot as i32)),
        Some(_) => Err(OdinRuntimeError::TypeMismatch { expected: "layout object", actual: "non-layout object".into() }),
        None => Err(OdinRuntimeError::ModuleLoad(format!("invalid object id {object_id}"))),
    }
}

/// 从堆还原显式类型化的 DOM 树：先校验布局身份，再按合同槽位序读出。
/// 不是从线性内存按偏移猜测；非法句柄与越出深度上限一律结构化失败。
pub(crate) fn read_tree(heap: &ObjectHeap, root: &Value) -> Result<DomNode, OdinRuntimeError> {
    read_tree_at(heap, root, 0)
}

fn read_tree_at(heap: &ObjectHeap, value: &Value, depth: usize) -> Result<DomNode, OdinRuntimeError> {
    if depth > MAX_READ_TREE_DEPTH {
        return Err(OdinRuntimeError::ModuleLoad(format!("DOM tree exceeds read limit {MAX_READ_TREE_DEPTH}")));
    }
    let (kind, object_id) = dom_node_parts(heap, value)?;
    if kind == DOM_KIND_TEXT {
        let text = read_string(&read_slot(heap, object_id, SLOT_TEXT)?, "text node content")?;
        return Ok(DomNode::Text(text));
    }
    if kind != DOM_KIND_ELEMENT {
        return Err(OdinRuntimeError::ModuleLoad(format!("unknown DOM node kind {kind}")));
    }
    let tag = read_string(&read_slot(heap, object_id, SLOT_TAG)?, "element tag")?;
    let text = read_string(&read_slot(heap, object_id, SLOT_TEXT)?, "element text")?;
    let attrs_value = read_slot(heap, object_id, SLOT_ATTRS)?;
    let children_value = read_slot(heap, object_id, SLOT_CHILDREN)?;

    let mut attrs = Vec::new();
    if let Value::Object(attrs_id) = attrs_value {
        let len = match heap.get(attrs_id) {
            Some(ObjectPayload::LayoutObject { slots, .. }) => slots.len(),
            _ => return Err(OdinRuntimeError::ModuleLoad(format!("invalid object id {attrs_id}"))),
        };
        for index in 0..len {
            let pair = array_runtime::array_get(heap, &attrs_value, &Value::I32(index as i32))?;
            let name = read_string(&array_runtime::array_get(heap, &pair, &Value::I32(0))?, "attribute name")?;
            let attr_value = read_string(&array_runtime::array_get(heap, &pair, &Value::I32(1))?, "attribute value")?;
            attrs.push((name, attr_value));
        }
    }

    let mut children = Vec::new();
    if let Value::Object(children_id) = children_value {
        let len = match heap.get(children_id) {
            Some(ObjectPayload::LayoutObject { slots, .. }) => slots.len(),
            _ => return Err(OdinRuntimeError::ModuleLoad(format!("invalid object id {children_id}"))),
        };
        for index in 0..len {
            let child = array_runtime::array_get(heap, &children_value, &Value::I32(index as i32))?;
            children.push(read_tree_at(heap, &child, depth + 1)?);
        }
    }

    Ok(DomNode::Element { tag, attrs, children, text })
}

/// `asgard.render` DOM 宿主 provider：把 `dom_*` 符号绑定到本模块的合同实现。
///
/// VM 内核不内置任何产品能力；本宿主（Asgard SSG）必须显式注册该 provider，
/// 未注册时含 DOM 导入的产物在加载期失败闭合。
#[derive(Debug, Default)]
pub(crate) struct DomProvider;

impl DomProvider {
    /// 构造可直接注册到 [`interpreter_vm_ovm::host::HostProvider`] 列表的绑定。
    pub(crate) fn registered() -> std::sync::Arc<dyn HostProvider> {
        std::sync::Arc::new(Self)
    }
}

/// 单个 `dom_*` 符号的绑定实体。
#[derive(Debug)]
struct DomImport {
    symbol: &'static str,
}

fn dom_string_arg(args: &[Value], index: usize, symbol: &str) -> Result<String, OdinRuntimeError> {
    let value = args.get(index).ok_or(OdinRuntimeError::StackUnderflow)?;
    match value {
        Value::String(text) => Ok(text.clone()),
        other => Err(OdinRuntimeError::TypeMismatch { expected: "utf8 string", actual: format!("`{other:?}` as `{symbol}` argument {index}") }),
    }
}

impl HostImport for DomImport {
    fn call(&self, args: &[Value], heap: &mut ObjectHeap) -> Result<Value, OdinRuntimeError> {
        match self.symbol {
            "dom_create_element" => create_element(heap, &dom_string_arg(args, 0, "dom_create_element")?),
            "dom_create_text" => create_text(heap, &dom_string_arg(args, 0, "dom_create_text")?),
            "dom_append" => {
                let parent = args.first().ok_or(OdinRuntimeError::StackUnderflow)?.clone();
                let child = args.get(1).ok_or(OdinRuntimeError::StackUnderflow)?.clone();
                append(heap, &parent, &child)
            }
            "dom_set_attr" => {
                let node = args.first().ok_or(OdinRuntimeError::StackUnderflow)?.clone();
                let name = dom_string_arg(args, 1, "dom_set_attr")?;
                let value = dom_string_arg(args, 2, "dom_set_attr")?;
                set_attr(heap, &node, &name, &value)
            }
            "dom_get_attr" => {
                let node = args.first().ok_or(OdinRuntimeError::StackUnderflow)?.clone();
                let name = dom_string_arg(args, 1, "dom_get_attr")?;
                get_attr(heap, &node, &name)
            }
            "dom_set_text" => {
                let node = args.first().ok_or(OdinRuntimeError::StackUnderflow)?.clone();
                let text = dom_string_arg(args, 1, "dom_set_text")?;
                set_text(heap, &node, &text)
            }
            other => Err(OdinRuntimeError::ModuleLoad(format!("unknown DOM provider symbol `{other}`"))),
        }
    }
}

const DOM_SYMBOLS: [&str; 6] = ["dom_create_element", "dom_create_text", "dom_append", "dom_set_attr", "dom_get_attr", "dom_set_text"];

impl HostProvider for DomProvider {
    fn resolve(&self, _module: &str, symbol: &str) -> Option<std::sync::Arc<dyn HostImport>> {
        let bound = DOM_SYMBOLS.iter().find(|name| **name == symbol)?;
        Some(std::sync::Arc::new(DomImport { symbol: *bound }) as std::sync::Arc<dyn HostImport>)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use interpreter_vm_ovm::host::parse_host_op;

    use super::*;

    fn s(text: &str) -> Value {
        Value::String(text.to_string())
    }

    /// 通过显式注册的 provider 调用一个 `dom_*` 绑定（与 VM 热路径同一分派合同）。
    fn dom_call(heap: &mut ObjectHeap, symbol: &str, args: &[Value]) -> Result<Value, OdinRuntimeError> {
        let provider: Arc<dyn HostProvider> = DomProvider::registered();
        let import = provider.resolve("odin.host", symbol).unwrap_or_else(|| panic!("provider 必须认领 `{symbol}`"));
        import.call(args, heap)
    }

    fn assert_underflow(result: Result<Value, OdinRuntimeError>, context: &str) {
        match result {
            Ok(value) => panic!("{context}: 缺参必须失败，实际成功返回 {value:?}"),
            Err(OdinRuntimeError::StackUnderflow) => {}
            Err(other) => panic!("{context}: 期望 StackUnderflow，实际 {other:?}"),
        }
    }

    #[test]
    fn dom_symbols_are_not_builtin_host_ops_and_provider_claims_them() {
        assert_eq!(parse_host_op("dom_create_element"), None, "DOM 不是 VM 内建宿主操作");
        let provider = DomProvider::registered();
        for symbol in ["dom_create_element", "dom_create_text", "dom_append", "dom_set_attr", "dom_get_attr", "dom_set_text"] {
            assert!(provider.resolve("odin.host", symbol).is_some(), "provider 必须认领 `{symbol}`");
        }
        assert!(provider.resolve("odin.host", "dom_create_fragment").is_none(), "未知符号必须保持未绑定");
    }

    #[test]
    fn build_tree_and_read_it_back_with_typed_layout_identity() {
        let mut heap = ObjectHeap::new();
        let root = dom_call(&mut heap, "dom_create_element", &[s("div")]).expect("create root");
        let child = dom_call(&mut heap, "dom_create_element", &[s("span")]).expect("create child");
        dom_call(&mut heap, "dom_set_attr", &[child.clone(), s("class"), s("hero")]).expect("set class");
        let text = dom_call(&mut heap, "dom_create_text", &[s("hello")]).expect("create text");
        dom_call(&mut heap, "dom_append", &[child.clone(), text]).expect("append text");
        dom_call(&mut heap, "dom_append", &[root.clone(), child]).expect("append child");

        let tree = read_tree(&heap, &root).expect("read tree");
        assert_eq!(
            tree,
            DomNode::Element {
                tag: "div".into(),
                attrs: vec![],
                children: vec![DomNode::Element {
                    tag: "span".into(),
                    attrs: vec![("class".into(), "hero".into())],
                    children: vec![DomNode::Text("hello".into())],
                    text: String::new(),
                }],
                text: String::new(),
            }
        );
    }

    #[test]
    fn set_attr_overwrites_same_name_and_get_attr_reads_back() {
        let mut heap = ObjectHeap::new();
        let node = create_element(&mut heap, "a").expect("create");
        set_attr(&mut heap, &node, "href", "/one").expect("set one");
        set_attr(&mut heap, &node, "href", "/two").expect("set two");
        set_attr(&mut heap, &node, "title", "t").expect("set title");
        assert_eq!(get_attr(&heap, &node, "href").expect("get href"), s("/two"));
        assert_eq!(get_attr(&heap, &node, "title").expect("get title"), s("t"));
        assert_eq!(get_attr(&heap, &node, "missing").expect("get missing"), s(""), "未设置的属性返回空串");

        let tree = read_tree(&heap, &node).expect("read");
        match tree {
            DomNode::Element { attrs, .. } => assert_eq!(attrs, vec![("href".into(), "/two".into()), ("title".into(), "t".into())]),
            other => panic!("expected element, got {other:?}"),
        }
    }

    #[test]
    fn set_text_replaces_children() {
        let mut heap = ObjectHeap::new();
        let node = create_element(&mut heap, "p").expect("create");
        let child = create_text(&mut heap, "old").expect("text");
        append(&mut heap, &node, &child).expect("append");
        set_text(&mut heap, &node, "new").expect("set text");
        let tree = read_tree(&heap, &node).expect("read");
        match tree {
            DomNode::Element { children, text, .. } => {
                assert!(children.is_empty(), "set_text 必须清空既有子节点: {children:?}");
                assert_eq!(text, "new");
            }
            other => panic!("expected element, got {other:?}"),
        }
    }

    #[test]
    fn invalid_handles_and_arguments_fail_structured() {
        let mut heap = ObjectHeap::new();
        let node = create_element(&mut heap, "div").expect("create");
        let text = create_text(&mut heap, "t").expect("text");

        let err = append(&mut heap, &text, &node).expect_err("文本节点不能作父节点");
        assert!(matches!(err, OdinRuntimeError::TypeMismatch { .. }));

        let err = append(&mut heap, &node, &Value::I32(7)).expect_err("整数不是 DOM 句柄");
        assert!(matches!(err, OdinRuntimeError::TypeMismatch { .. }));

        let err = set_attr(&mut heap, &node, "", "v").expect_err("空属性名必须拒绝");
        assert!(matches!(err, OdinRuntimeError::TypeMismatch { .. }));

        let err = create_element(&mut heap, "").expect_err("空标签名必须拒绝");
        assert!(matches!(err, OdinRuntimeError::TypeMismatch { .. }));

        let err = dom_call(&mut heap, "dom_create_element", &[Value::I32(1)]).expect_err("非字符串标签必须拒绝");
        assert!(matches!(err, OdinRuntimeError::TypeMismatch { .. }));
    }

    #[test]
    fn foreign_layout_object_is_not_a_dom_handle() {
        let mut heap = ObjectHeap::new();
        let plain = Value::Object(heap.alloc_layout_object(0, 1));
        let err = append(&mut heap, &plain, &plain).expect_err("外来布局对象不得冒充 DOM 句柄");
        assert!(matches!(err, OdinRuntimeError::TypeMismatch { .. }));

        let dom = create_element(&mut heap, "div").expect("create");
        if let Value::Object(id) = &dom {
            match heap.get(*id) {
                Some(ObjectPayload::LayoutObject { layout_id, .. }) => assert_eq!(*layout_id, DOM_NODE_LAYOUT_ID),
                other => panic!("expected layout object, got {other:?}"),
            }
        }
    }

    /// 宿主 provider 的严格元数合同（随实现从 VM `host_strict_args` 迁入）：
    /// 固定元数的 `dom_*` 符号缺参一律 `StackUnderflow`，不得用默认值继续。
    #[test]
    fn dom_provider_ops_require_arguments() {
        let provider = DomProvider::registered();
        let dom = |symbol: &str| provider.resolve("odin.host", symbol).unwrap_or_else(|| panic!("provider 必须认领 `{symbol}`"));

        let mut heap = ObjectHeap::new();
        let element = dom("dom_create_element").call(&[Value::String("div".to_string())], &mut heap).expect("创建元素");
        assert_underflow(dom("dom_append").call(&[element.clone()], &mut heap), "dom_append 缺子节点");
        assert_underflow(dom("dom_append").call(&[], &mut heap), "dom_append 零参");
        assert_underflow(dom("dom_set_attr").call(&[element.clone(), Value::String("id".to_string())], &mut heap), "dom_set_attr 缺值");
        assert_underflow(dom("dom_set_attr").call(&[], &mut heap), "dom_set_attr 零参");
        assert_underflow(dom("dom_get_attr").call(&[element.clone()], &mut heap), "dom_get_attr 缺属性名");
        assert_underflow(dom("dom_get_attr").call(&[], &mut heap), "dom_get_attr 零参");
        assert_underflow(dom("dom_set_text").call(&[element.clone()], &mut heap), "dom_set_text 缺文本");
        assert_underflow(dom("dom_set_text").call(&[], &mut heap), "dom_set_text 零参");
        // 正例：文本节点本身是合法载体，补全参数后追加成功。
        let text = dom("dom_create_text").call(&[Value::String("hi".to_string())], &mut heap).expect("创建文本");
        assert!(dom("dom_append").call(&[element.clone(), text], &mut heap).is_ok());
        assert!(dom("dom_set_text").call(&[element, Value::String("new".to_string())], &mut heap).is_ok());
    }
}
