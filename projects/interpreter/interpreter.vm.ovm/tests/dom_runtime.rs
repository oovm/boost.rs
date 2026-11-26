//! `asgard.render` 内存 DOM 宿主：合同操作的堆表示与读出 ABI 测试。

use std::sync::Arc;

use interpreter_gc::{ObjectHeap, ObjectPayload, Value};
use interpreter_vm_ovm::{
    dom_runtime::{self, DOM_NODE_LAYOUT_ID, DomNode, DomProvider},
    error::OdinRuntimeError,
    host::{HostProvider, parse_host_op},
};

fn s(text: &str) -> Value {
    Value::String(text.to_string())
}

/// 通过显式注册的 provider 调用一个 `dom_*` 绑定（与 VM 热路径同一分派合同）。
fn dom_call(heap: &mut ObjectHeap, symbol: &str, args: &[Value]) -> Result<Value, OdinRuntimeError> {
    let provider: Arc<dyn HostProvider> = DomProvider::registered();
    let import = provider.resolve("odin.host", symbol).unwrap_or_else(|| panic!("provider 必须认领 `{symbol}`"));
    import.call(args, heap)
}

#[test]
fn dom_symbols_are_not_builtin_host_ops_and_provider_claims_them() {
    assert_eq!(parse_host_op("dom_create_element"), None, "DOM 不再是 VM 内建宿主操作");
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

    let tree = dom_runtime::read_tree(&heap, &root).expect("read tree");
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
    let node = dom_runtime::create_element(&mut heap, "a").expect("create");
    dom_runtime::set_attr(&mut heap, &node, "href", "/one").expect("set one");
    dom_runtime::set_attr(&mut heap, &node, "href", "/two").expect("set two");
    dom_runtime::set_attr(&mut heap, &node, "title", "t").expect("set title");
    assert_eq!(dom_runtime::get_attr(&heap, &node, "href").expect("get href"), s("/two"));
    assert_eq!(dom_runtime::get_attr(&heap, &node, "title").expect("get title"), s("t"));
    assert_eq!(dom_runtime::get_attr(&heap, &node, "missing").expect("get missing"), s(""), "未设置的属性返回空串");

    let tree = dom_runtime::read_tree(&heap, &node).expect("read");
    match tree {
        DomNode::Element { attrs, .. } => assert_eq!(attrs, vec![("href".into(), "/two".into()), ("title".into(), "t".into())]),
        other => panic!("expected element, got {other:?}"),
    }
}

#[test]
fn set_text_replaces_children() {
    let mut heap = ObjectHeap::new();
    let node = dom_runtime::create_element(&mut heap, "p").expect("create");
    let child = dom_runtime::create_text(&mut heap, "old").expect("text");
    dom_runtime::append(&mut heap, &node, &child).expect("append");
    dom_runtime::set_text(&mut heap, &node, "new").expect("set text");
    let tree = dom_runtime::read_tree(&heap, &node).expect("read");
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
    let node = dom_runtime::create_element(&mut heap, "div").expect("create");
    let text = dom_runtime::create_text(&mut heap, "t").expect("text");

    let err = dom_runtime::append(&mut heap, &text, &node).expect_err("文本节点不能作父节点");
    assert!(matches!(err, OdinRuntimeError::TypeMismatch { .. }));

    let err = dom_runtime::append(&mut heap, &node, &Value::I32(7)).expect_err("整数不是 DOM 句柄");
    assert!(matches!(err, OdinRuntimeError::TypeMismatch { .. }));

    let err = dom_runtime::set_attr(&mut heap, &node, "", "v").expect_err("空属性名必须拒绝");
    assert!(matches!(err, OdinRuntimeError::TypeMismatch { .. }));

    let err = dom_runtime::create_element(&mut heap, "").expect_err("空标签名必须拒绝");
    assert!(matches!(err, OdinRuntimeError::TypeMismatch { .. }));

    let err = dom_call(&mut heap, "dom_create_element", &[Value::I32(1)]).expect_err("非字符串标签必须拒绝");
    assert!(matches!(err, OdinRuntimeError::TypeMismatch { .. }));
}

#[test]
fn foreign_layout_object_is_not_a_dom_handle() {
    let mut heap = ObjectHeap::new();
    let plain = Value::Object(heap.alloc_layout_object(0, 1));
    let err = dom_runtime::append(&mut heap, &plain, &plain).expect_err("外来布局对象不得冒充 DOM 句柄");
    assert!(matches!(err, OdinRuntimeError::TypeMismatch { .. }));

    let dom = dom_runtime::create_element(&mut heap, "div").expect("create");
    if let Value::Object(id) = &dom {
        match heap.get(*id) {
            Some(ObjectPayload::LayoutObject { layout_id, .. }) => assert_eq!(*layout_id, DOM_NODE_LAYOUT_ID),
            other => panic!("expected layout object, got {other:?}"),
        }
    }
}
