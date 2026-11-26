//! Invariants of the CPython opcode tables used by the pyc decoder.

use data_binary_container::pyc_opcodes::tables;

fn assert_sorted_unique(label: &str, table: &[(u8, &str)]) {
    assert!(!table.is_empty(), "{label} is empty");
    for pair in table.windows(2) {
        assert!(pair[0].0 < pair[1].0, "{label} is not sorted at {}", pair[0].0);
        assert!(!pair[0].1.is_empty(), "{label} has an empty name");
    }
}

#[test]
fn tables_are_sorted_and_unique() {
    for (version, table) in tables() {
        assert_sorted_unique(&format!("3.{}", version % 100), table);
    }
}

#[test]
fn every_table_names_cache_and_resume() {
    for (_, table) in tables() {
        assert_eq!(table[0], (0, "CACHE"));
        assert!(table.iter().any(|(_, name)| *name == "RESUME"));
        assert!(table.iter().any(|(_, name)| *name == "LOAD_CONST"));
    }
}
