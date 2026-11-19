use data_text_toml::language::{from_str, to_string};

#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
struct TestStruct {
    name: String,
    age: u32,
    hobbies: Vec<String>,
    address: Address,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
struct Address {
    street: String,
    city: String,
    zip: String,
}

#[test]
fn test_serialize_struct() {
    let test_data = TestStruct {
        name: "John Doe".to_string(),
        age: 30,
        hobbies: vec!["reading".to_string(), "hiking".to_string(), "coding".to_string()],
        address: Address { street: "123 Main St".to_string(), city: "New York".to_string(), zip: "10001".to_string() },
    };

    let toml = to_string(&test_data).unwrap();
    println!("Serialized TOML:\n{}", toml);

    let deserialized: TestStruct = from_str(&toml).unwrap();
    assert_eq!(test_data, deserialized);
}

#[test]
fn test_serialize_primitive_types() {
    // Test string
    let s = "hello world";
    let toml = to_string(&s).unwrap();
    println!("Serialized string: {}", toml);
    let deserialized: String = from_str(&toml).unwrap();
    assert_eq!(s, deserialized);

    // Test number
    let n = 42;
    let toml = to_string(&n).unwrap();
    println!("Serialized number: {}", toml);
    let deserialized: i32 = from_str(&toml).unwrap();
    assert_eq!(n, deserialized);

    // Test boolean
    let b = true;
    let toml = to_string(&b).unwrap();
    println!("Serialized boolean: {}", toml);
    let deserialized: bool = from_str(&toml).unwrap();
    assert_eq!(b, deserialized);
}

#[test]
fn test_serialize_collections() {
    // Test vector
    let v = vec![1, 2, 3, 4, 5];
    let toml = to_string(&v).unwrap();
    println!("Serialized vector:\n{}", toml);
    let deserialized: Vec<i32> = from_str(&toml).unwrap();
    assert_eq!(v, deserialized);

    // Test map
    let mut map = std::collections::HashMap::new();
    map.insert("a".to_string(), 1);
    map.insert("b".to_string(), 2);
    map.insert("c".to_string(), 3);
    let toml = to_string(&map).unwrap();
    println!("Serialized map:\n{}", toml);
    let deserialized: std::collections::HashMap<String, i32> = from_str(&toml).unwrap();
    assert_eq!(map, deserialized);
}

#[derive(Debug, serde::Deserialize, PartialEq)]
struct NestedDoc {
    title: String,
    physical: Physical,
    owner: Owner,
    products: Vec<Product>,
}

#[derive(Debug, serde::Deserialize, PartialEq)]
struct Physical {
    color: String,
}

#[derive(Debug, serde::Deserialize, PartialEq)]
struct Owner {
    name: String,
    dob: String,
    rating: f64,
}

#[derive(Debug, serde::Deserialize, PartialEq)]
struct Product {
    name: String,
    sku: i64,
    color: Option<String>,
}

const NESTED_DOC: &str = r#"
title = "Example"
physical.color = "orange"

[owner]
name = "Tom"
dob = 1979-05-27T07:32:00Z
rating = 4.5

[[products]]
name = "Hammer"
sku = 738594937

[[products]]
name = "Nail"
sku = 284758393
color = "gray"
"#;

#[test]
fn test_deserialize_nested_documents() {
    let parsed: NestedDoc = from_str(NESTED_DOC).unwrap();

    assert_eq!(parsed.title, "Example");
    assert_eq!(parsed.physical.color, "orange");
    assert_eq!(parsed.owner.name, "Tom");
    assert_eq!(parsed.owner.dob, "1979-05-27T07:32:00Z");
    assert_eq!(parsed.owner.rating, 4.5);
    assert_eq!(parsed.products.len(), 2);
    assert_eq!(parsed.products[0], Product { name: "Hammer".into(), sku: 738594937, color: None });
    assert_eq!(parsed.products[1], Product { name: "Nail".into(), sku: 284758393, color: Some("gray".into()) });
}

#[test]
fn test_deserialize_scalar_types() {
    #[derive(Debug, serde::Deserialize, PartialEq)]
    struct Scalars {
        integer: i64,
        hex: i64,
        float: f64,
        boolean: bool,
        literal: String,
        escaped: String,
        multiline: String,
    }

    let document = r#"
integer = 1_000
hex = 0xDEAD_BEEF
float = 6.626e-34
boolean = true
literal = 'C:\Users\nodejs'
escaped = "tab:\t quote:\""
multiline = """
first
second"""
"#;

    let parsed: Scalars = from_str(document).unwrap();
    assert_eq!(parsed.integer, 1000);
    assert_eq!(parsed.hex, 0xDEAD_BEEF);
    assert_eq!(parsed.float, 6.626e-34);
    assert!(parsed.boolean);
    assert_eq!(parsed.literal, r"C:\Users\nodejs");
    assert_eq!(parsed.escaped, "tab:\t quote:\"");
    assert_eq!(parsed.multiline, "first\nsecond");
}

#[test]
fn test_round_trip_nested_tables() {
    #[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
    struct Site {
        title: String,
        owner: Owner,
        tags: Vec<String>,
    }

    #[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
    struct Owner {
        name: String,
        age: u8,
    }

    let site = Site { title: "Docs".into(), owner: Owner { name: "Ada".into(), age: 36 }, tags: vec!["rust".into(), "toml".into()] };

    let document = to_string(&site).unwrap();
    println!("Serialized nested TOML:\n{}", document);
    assert!(document.contains("[owner]"), "nested tables use section headers:\n{document}");

    let parsed: Site = from_str(&document).unwrap();
    assert_eq!(site, parsed);
}
