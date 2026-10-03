# mapKeys2 Testing Instructions

## Status
✅ **Implementation COMPLETE** - All code changes have been made
⏳ **Testing PENDING** - Requires `cargo` to rebuild the project

## What Was Implemented

`mapKeys2` is a new Cagara primitive that transforms column names using **user-defined functions** instead of regex patterns.

### Syntax
```cagara
# Old regex-based approach
users & mapKeys "^" "u_"

# New function-based approach
users & mapKeys2 (\x => "u_" <> x)
```

### Examples
```cagara
# Prefix
users & mapKeys2 (\x => "u_" <> x)
# {id, name} -> {u_id, u_name}

# Suffix
users & mapKeys2 (\x => x <> "_old")
# {id, name} -> {id_old, name_old}

# Conditional
users & mapKeys2 (\x => if x == "id" then "user_id" else x)
# {id, name} -> {user_id, name}

# Nested operations
users & mapKeys2 (\x => "pre_" <> x <> "_post")
# {id, name} -> {pre_id_post, pre_name_post}
```

## Files Modified

1. ✅ `crates/cagara-hir/src/value.rs` - Added MapKeys2 primitive
2. ✅ `crates/cagara-hir/src/check.rs` - Added type checking support
3. ✅ `prelude.cagara` - Added surface syntax

## Files Created for Testing

1. `test_mapkeys2.cagara` - Manual test cases
2. `mapkeys2_tests.rs` - Unit tests (need to be added to tests.rs)
3. `test_mapkeys2.sh` - Automated test script
4. `MAPKEYS2_IMPLEMENTATION.md` - Implementation documentation

## How to Test (Once Cargo is Available)

### Quick Test
```bash
# 1. Rebuild the project
cargo build

# 2. Run the automated test suite
./test_mapkeys2.sh
```

### Manual Testing
```bash
# 1. Build
cargo build

# 2. Test type checking
./target/debug/cagara check test_mapkeys2.cagara

# 3. Test individual examples
echo 'users = table "db" "users"
result = users & mapKeys2 (\x => "u_" <> x)' | ./target/debug/cagara check -
```

### Unit Testing
```bash
# Add the tests from mapkeys2_tests.rs to:
# crates/cagara-hir/src/check/tests.rs

# Then run:
cargo test map_keys2
```

## Expected Behavior

### Success Cases
- ✓ Simple transformations (prefix, suffix, concatenation)
- ✓ Conditional expressions
- ✓ String comparisons
- ✓ Identity function `(\x => x)`
- ✓ Nested string operations

### Error Cases (Should Fail with Clear Messages)
- ✗ Empty column names: `(\x => "")` 
- ✗ Duplicate column names: `(\x => "same")`
- ✗ Non-pure operations: `(\x => sum .amount)`
- ✗ Query references: `(\x => .other_field)`

## Implementation Notes

The function is evaluated **at compile-time** during type checking:
- Only pure string operations are allowed
- The function must be deterministic
- Column names are computed before SQL generation
- The implementation reuses existing `KeyMap::Function` infrastructure

## Installing Rust (If Needed)

If cargo is not available, install Rust:
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
```

Then return to this directory and run `./test_mapkeys2.sh`.

## Next Steps After Testing

1. ✅ Verify all tests pass
2. ✅ Add unit tests to the test suite
3. ✅ Test SQL generation output
4. ✅ Update documentation with mapKeys2 examples
5. ✅ Consider adding more string functions (substring, replace, etc.)

## Technical Details

See `MAPKEYS2_IMPLEMENTATION.md` for complete implementation details including:
- Architecture overview
- Code changes with line numbers
- Type system integration
- Compile-time evaluation strategy
