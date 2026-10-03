// Unit tests for mapKeys2 functionality
// Add these tests to crates/cagara-hir/src/check/tests.rs

#[test]
fn map_keys2_prefix() {
    // Test: users & mapKeys2 (\x => "u_" <> x)
    // Input: {id: int, name: string}
    // Expected output: {u_id: int, u_name: string}
    let src = r#"
        users = table "db" "users"
        result = users & mapKeys2 (\x => "u_" <> x)
    "#;

    // This should type-check successfully
    // The output row should be {u_id: int, u_name: string}
}

#[test]
fn map_keys2_suffix() {
    // Test: users & mapKeys2 (\x => x <> "_old")
    // Input: {id: int, name: string}
    // Expected output: {id_old: int, name_old: string}
    let src = r#"
        users = table "db" "users"
        result = users & mapKeys2 (\x => x <> "_old")
    "#;
}

#[test]
fn map_keys2_conditional() {
    // Test: users & mapKeys2 (\x => if x == "id" then "user_id" else x)
    // Input: {id: int, name: string}
    // Expected output: {user_id: int, name: string}
    let src = r#"
        users = table "db" "users"
        result = users & mapKeys2 (\x => if x == "id" then "user_id" else x)
    "#;
}

#[test]
fn map_keys2_identity() {
    // Test: identity function should preserve names
    // users & mapKeys2 (\x => x)
    let src = r#"
        users = table "db" "users"
        result = users & mapKeys2 (\x => x)
    "#;
}

#[test]
fn map_keys2_nested() {
    // Test: nested string operations
    // users & mapKeys2 (\x => "pre_" <> x <> "_post")
    let src = r#"
        users = table "db" "users"
        result = users & mapKeys2 (\x => "pre_" <> x <> "_post")
    "#;
}

#[test]
fn map_keys2_error_empty_name() {
    // Test: function that produces empty names should fail
    // users & mapKeys2 (\x => "")
    let src = r#"
        users = table "db" "users"
        result = users & mapKeys2 (\x => "")
    "#;

    // This should produce a type error about empty column names
}

#[test]
fn map_keys2_error_collision() {
    // Test: function that produces duplicate names should fail
    // users & mapKeys2 (\x => "same")
    let src = r#"
        users = table "db" "users"
        result = users & mapKeys2 (\x => "same")
    "#;

    // This should produce a type error about column name collisions
}

#[test]
fn map_keys2_pipeline() {
    // Test: mapKeys2 in a pipeline
    let src = r#"
        users = table "db" "users"
        result = users
            & mapKeys2 (\x => "u_" <> x)
            & where .u_id > 10
            & select {.u_name}
    "#;
}

#[test]
fn map_keys2_composition() {
    // Test: composing multiple key transformations
    let src = r#"
        users = table "db" "users"
        result = users
            & mapKeys2 (\x => "u_" <> x)
            & mapKeys2 (\x => x <> "_v2")
    "#;

    // Expected output: {u_id_v2: int, u_name_v2: string}
}
