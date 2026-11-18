//! 接收端合同门：已实现方法逐条落到接收端，未实现构造必须显式返回不支持。

use compiler_specialize::ResidualSink;

/// 记录所有调用的测试接收端。
#[derive(Default)]
struct Recorder {
    ops: Vec<String>,
}

impl ResidualSink for Recorder {
    fn push_i64(&mut self, value: i64) {
        self.ops.push(format!("i64:{value}"));
    }
    fn push_bool(&mut self, value: bool) {
        self.ops.push(format!("bool:{value}"));
    }
    fn push_string(&mut self, value: &str) {
        self.ops.push(format!("str:{value}"));
    }
    fn push_nil(&mut self) {
        self.ops.push("nil".into());
    }
    fn load(&mut self, name: &str) {
        self.ops.push(format!("load:{name}"));
    }
    fn store(&mut self, name: &str) {
        self.ops.push(format!("store:{name}"));
    }
    fn pop(&mut self) {
        self.ops.push("pop".into());
    }
    fn add(&mut self) {
        self.ops.push("add".into());
    }
    fn sub(&mut self) {
        self.ops.push("sub".into());
    }
    fn mul(&mut self) {
        self.ops.push("mul".into());
    }
    fn div(&mut self) {
        self.ops.push("div".into());
    }
    fn rem(&mut self) {
        self.ops.push("rem".into());
    }
    fn eq(&mut self) {
        self.ops.push("eq".into());
    }
    fn ne(&mut self) {
        self.ops.push("ne".into());
    }
    fn lt(&mut self) {
        self.ops.push("lt".into());
    }
    fn le(&mut self) {
        self.ops.push("le".into());
    }
    fn gt(&mut self) {
        self.ops.push("gt".into());
    }
    fn ge(&mut self) {
        self.ops.push("ge".into());
    }
    fn concat(&mut self) {
        self.ops.push("concat".into());
    }
    fn not(&mut self) {
        self.ops.push("not".into());
    }
    fn print(&mut self, argc: usize) {
        self.ops.push(format!("print:{argc}"));
    }
    fn label(&mut self, name: &str) {
        self.ops.push(format!("label:{name}"));
    }
    fn jump(&mut self, label: &str) {
        self.ops.push(format!("jump:{label}"));
    }
    fn jump_if_false(&mut self, label: &str) {
        self.ops.push(format!("jf:{label}"));
    }
    fn jump_if_true(&mut self, label: &str) {
        self.ops.push(format!("jt:{label}"));
    }
    fn begin_function(&mut self, name: &str) {
        self.ops.push(format!("fn:{name}"));
    }
    fn add_parameter(&mut self, name: &str) {
        self.ops.push(format!("param:{name}"));
    }
    fn end_function(&mut self) {
        self.ops.push("endfn".into());
    }
    fn call(&mut self, name: &str, argc: usize) {
        self.ops.push(format!("call:{name}/{argc}"));
    }
    fn ret(&mut self) {
        self.ops.push("ret".into());
    }
}

#[test]
fn implemented_operations_are_recorded_in_order() {
    let mut sink = Recorder::default();
    sink.begin_function("main");
    sink.push_i64(42);
    sink.store("x");
    sink.load("x");
    sink.push_bool(true);
    sink.add();
    sink.ret();
    sink.end_function();
    assert_eq!(sink.ops, vec!["fn:main", "i64:42", "store:x", "load:x", "bool:true", "add", "ret", "endfn"]);
}

#[test]
fn optional_constructs_fail_closed_by_default() {
    let mut sink = Recorder::default();
    assert!(sink.table_new().is_err());
    assert!(sink.table_field_get("width").is_err());
    assert!(sink.dynamic_call("method", 2).is_err());
    let message = sink.unsupported("custom op").unwrap_err();
    assert!(message.contains("custom op"));
    assert!(sink.ops.is_empty());
}
