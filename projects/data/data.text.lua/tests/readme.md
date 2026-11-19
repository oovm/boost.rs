# 🧪 Lua Parser Test Suite

`data-text-lua` utilizes a multi-layered testing strategy to ensure high fidelity parsing across various Lua environments (Lua 5.1 to 5.4).

## 📊 Test Coverage

### 1. Lexer Snapshot Tests
Ensures tokens are correctly identified, including edge cases like:
- **Long Strings/Comments**: `[[ ... ]]`, `[=[ ... ]=]`, etc.
- **Escape Sequences**: Hex, decimal, and standard escapes in strings.
- **Numeric Literals**: Hexadecimal floats and standard scientific notation.

### 2. Syntax Verification
Validates the structural integrity of the AST for:
- **Table Constructors**: Handles nested tables, explicit keys, and implicit array indices.
- **Function Scope**: Proper nesting of global and local functions.
- **Control Flow**: Complex `if-elseif-else` chains, `for` loops (numeric and generic), and `repeat-until` blocks.
- **Operator Precedence**: Ensures Lua-specific operator behavior (e.g., right-associative exponentiation `^` and concatenation `..`).

### 3. Error Recovery Tests
Verifies that the parser remains stable when encountering:
- Missing `end` tokens in deeply nested structures.
- Incomplete table definitions.
- Syntax errors inside function bodies.

## 🚀 Running Tests

```bash
# Run all Lua parser tests
cargo test -p data-text-lua

# Run specific lexer tests
cargo test -p data-text-lua --test lexer_tests
```

## 📈 Quality Standards
Every pull request must pass the full test suite and maintain snapshot stability. For new language features or bug fixes, corresponding tests should be added to the `tests/` directory.
