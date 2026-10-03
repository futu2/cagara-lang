# mapKeys2 Implementation Summary

## Implementation Status: COMPLETE

All code changes have been implemented according to the plan. The implementation is ready for testing once the project is rebuilt with `cargo build`.

## Changes Made

### 1. Added MapKeys2 Primitive (`crates/cagara-hir/src/value.rs`)
- ✅ Added `MapKeys2` variant to `Prim` enum
- ✅ Added `("__mapKeys2", Prim::MapKeys2)` to `PRIMS` constant
- ✅ Added arity case: `MapKeys2 => 2`

### 2. Extended KeyMap Enum (`crates/cagara-hir/src/check.rs`)
- ✅ `Function(Box<ast::Expr>)` variant already exists in KeyMap enum
- ✅ `apply()` method already handles Function variant with compile-time evaluation
- ✅ `eval_key_function()` already implemented for compile-time evaluation

### 3. Added Primitive Type (`crates/cagara-hir/src/check.rs`)
- ✅ Added MapKeys2 case in `prim_type()` function (line ~2534)
- ✅ Returns: `fun(con("key_function"), fun(query(r), query(out)))`

### 4. Updated Marker Detection (`crates/cagara-hir/src/check.rs`)
- ✅ Added "key_function" case to `key_marker()` function
- ✅ Added error message in `key_marker_msg()` function

### 5. Updated Key Capturing (`crates/cagara-hir/src/check.rs`)
- ✅ Added "key_function" to the list of markers that capture expressions (line ~2146)
- ✅ Uses existing `captured_keymap_expr` field to store the function

### 6. Added Key Stage Handler (`crates/cagara-hir/src/check.rs`)
- ✅ Added `[("key_function", _)]` case in `key_stage()` function
- ✅ Creates `KeyMap::Function` and unifies with output type

### 7. Added Surface Syntax (`prelude.cagara`)
- ✅ Added `mapKeys2 = __mapKeys2` binding with documentation

### 8. SQL Lowering
- ✅ No changes needed - the lowering already handles MapKey types generically

## Test File Created

Created `test_mapkeys2.cagara` with three test cases:
1. Simple prefix: `users & mapKeys2 (\x => "u_" <> x)`
2. Conditional: `users & mapKeys2 (\x => if x == "id" then "user_id" else x)`
3. Suffix: `users & mapKeys2 (\x => x <> "_old")`

## How to Test

Once cargo is available:

```bash
# 1. Build the project
cargo build

# 2. Run the type checker on the test file
./target/debug/cagara check test_mapkeys2.cagara

# 3. Run unit tests
cargo test map_keys2
```

Expected behavior:
- The type checker should successfully infer the output row types
- Column names should be transformed according to the lambda functions
- Error messages should appear for invalid functions (non-pure operations)

## Implementation Architecture

The implementation follows the existing pattern for function-based key mapping:

1. **Type-checking time**: The function expression is captured when `mapKeys2` is applied
2. **Type reduction**: When the input row is closed, `reduce_mapkey()` evaluates the function for each column name using `eval_key_function()`
3. **Compile-time evaluation**: The function must be pure (string operations only) and deterministic
4. **SQL generation**: The final column names are already computed during type-checking, so SQL lowering just uses them directly

## Supported Function Operations

The `eval_key_function()` already supports:
- String literals: `"prefix_"`
- Concatenation: `x <> "_suffix"`
- Parameter reference: `x` (the input column name)
- Conditionals: `if x == "id" then "user_id" else x`
- String comparisons: `==`, `!=`

## Validation

The implementation validates:
- No empty column names produced
- No column name collisions
- Function must be evaluable at compile-time (pure operations only)

## Notes

- The Function variant was already implemented in the KeyMap enum
- The compile-time evaluator `eval_key_function()` was already implemented
- This implementation reuses all existing infrastructure
- Only needed to wire up the new MapKeys2 primitive through the type system
