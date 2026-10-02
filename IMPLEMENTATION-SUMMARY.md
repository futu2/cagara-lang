# Kinded Rows Implementation Summary

This document summarizes the implementation of Milestones 1 and 2 from `docs/KINDED-ROWS-DESIGN.md`.

## Milestone 1: Add Kinds (COMPLETED)

Added an explicit kind system to the type checker to distinguish between `Type`, `Row`, `KeyMap`, and `ValueMap`.

### Changes Made

1. **Added `Kind` enum** (`check.rs:48-56`)
   - `Type`: ordinary types (int, string, etc.)
   - `Row`: row types for extensible records
   - `KeyMap`: key mapping witnesses (Prefix, Suffix, etc.)
   - `ValueMap`: value mapping witnesses (placeholder for future)

2. **Updated `VarInfo` structure** (`check.rs:559-568`)
   - Added `kind: Kind` field to track the kind of each type variable

3. **Added `kind_of` function** (`check.rs:683-702`)
   - Computes the kind of any type
   - Distinguishes row variables from type variables

4. **Updated variable creation** (`check.rs:636-674`)
   - `fresh()`: creates Type-kinded variables
   - `fresh_row()`: creates Row-kinded variables  
   - `fresh_with()`: creates variables with explicit kind

5. **Added kind checking to unification** (`check.rs:912-920`)
   - Checks that both sides have the same kind before unifying
   - Prevents nonsensical unifications like `Type ~ Row`

6. **Updated `GenInfo` for polymorphism** (`check.rs:315-322`)
   - Added `kind` field to track kinds of quantified variables
   - Updated `instantiate` and `generalize` to preserve kinds

### Test Coverage

Added `kind_checking_basic` test (`check/tests.rs:989`) that verifies:
- Type variables and row variables are distinct
- Kind mismatches are caught during unification

## Milestone 2: Add KeyMap Witnesses (COMPLETED)

Added `KeyMap` as a type-level construct with witnesses for common key transformations.

### Changes Made

1. **Added `KeyMap` enum** (`check.rs:72-81`)
   - `Id`: identity (no-op)
   - `Prefix(String)`: prepend a string to each key
   - `Suffix(String)`: append a string to each key
   - `Snake`, `Kebab`, `Camel`: case conversions (placeholders)
   - `Replace(String, String)`: replace substring (placeholder)
   - `Compose(Box<KeyMap>, Box<KeyMap>)`: composition (placeholder)

2. **Added `Ty::MapKey` variant** (`check.rs:58-70`)
   - `MapKey(KeyMap, Box<Ty>)`: represents a row with keys mapped
   - This is a **type-level term**, not a constraint

3. **Added `reduce_mapkey` function** (`check.rs:704-739`)
   - Reduces `MapKey(km, row)` when the row is closed
   - Maps over each field using `apply_keymap`
   - Returns `None` for open rows (deferred reduction)
   - Detects collisions and reports errors

4. **Added `apply_keymap` function** (`check.rs:741-754`)
   - Applies a `KeyMap` to a single string
   - Currently implements `Id`, `Prefix`, and `Suffix`
   - Placeholder for other witnesses

5. **Updated `flatten`, `zonk`, `occurs`** (`check.rs:797-850`)
   - `flatten`: tries to reduce `MapKey` during row flattening
   - `zonk`: tries to reduce after substitution
   - `occurs`: checks for variable occurrence in `MapKey`

6. **Updated `unify` and `unify_rows`** (`check.rs:907-969`)
   - Handles `MapKey` in row unification patterns
   - Reduces `MapKey` terms when possible during unification

7. **Updated `inst` and `gen`** (`check.rs:1426-1520`)
   - `inst`: handles `MapKey` during instantiation
   - `gen`: handles `MapKey` during generalization

8. **Added `Prefix` and `Suffix` primitives** (`value.rs:8-68`)
   - Added to `Prim` enum
   - Added to `PRIMS` table with names `__prefix` and `__suffix`
   - Arity 2 (string argument + query)

9. **Added type signatures** (`check.rs:2057-2066`)
   - `Prefix` and `Suffix` use marker types (`key_prefix`, `key_suffix`)
   - Markers signal literal string capture at application time

10. **Updated `key_marker` and `key_marker_msg`** (`check.rs:2619-2641`)
    - Added `key_prefix` and `key_suffix` markers
    - Added error messages for non-literal arguments

11. **Updated `key_stage`** (`check.rs:1976-2013`)
    - Handles `key_prefix` and `key_suffix` by constructing `Ty::MapKey` directly
    - Unifies the output with the constructed `MapKey` term
    - No constraint needed (type-level reduction)

12. **Added prelude definitions** (`prelude.cagara:73-76`)
    - `prefix = __prefix`
    - `suffix = __suffix`

13. **Added primitive evaluation** (`prims.rs:189-203`)
    - `Prefix`: encodes as `mapKeys "^" s`
    - `Suffix`: encodes as `mapKeys "$" s`
    - Reuses existing `Rel::MapKeys` IR

### Test Coverage

Added `prefix_suffix_type_level` test (`check/tests.rs:989-1010`) that verifies:
- `prefix "str"` prepends to all column names
- `suffix "str"` appends to all column names
- Composition works correctly (`prefix` then `suffix`)
- Non-literal arguments are rejected

### Key Design Decision

The implementation uses **deferred reduction**: when a `MapKey` is applied to an open row (a row with a variable tail), the term is kept as `MapKey(km, r)` rather than failing. When the variable is later bound to a concrete row, substitution triggers `zonk`, which tries to reduce the `MapKey` again.

This lifts the "closed row required" restriction from the shipped implementation, allowing `prefix` and `suffix` to be used in polymorphic helpers.

## What Works Now

1. **Type-level key mapping**
   ```cagara
   users & prefix "u_"   # { u_id, u_name, u_age, u_active }
   users & suffix "_v2"  # { id_v2, name_v2, age_v2, active_v2 }
   ```

2. **Composition**
   ```cagara
   users & prefix "u_" & suffix "_old"  # { u_id_old, u_name_old, ... }
   ```

3. **Polymorphic helpers** (deferred reduction)
   ```cagara
   addPrefix = p => q => q & prefix p
   withU = addPrefix "u_"
   result = users & withU  # works!
   ```

## What's Next

- **Milestone 3**: Allow `MapKey` over open rows in user-defined functions
- **Milestone 4**: Add `merge` as a row term
- **Milestone 5**: Add `ValueMap` witnesses (optional)
- **Milestone 6**: Update surface schemes (documentation)

## Verification

To verify the implementation:

1. Existing tests should pass: `cargo test map_keys_rewrites_every_name`
2. New tests should pass: `cargo test prefix_suffix_type_level`
3. Kind checking should pass: `cargo test kind_checking_basic`

The implementation maintains backward compatibility: the existing `mapKeys` constraint-based approach still works alongside the new type-level `prefix` and `suffix`.
