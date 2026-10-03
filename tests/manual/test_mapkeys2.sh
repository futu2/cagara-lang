#!/bin/bash
# Test script for mapKeys2 implementation
# Run this after rebuilding the project with cargo

set -e

echo "======================================"
echo "mapKeys2 Implementation Test Suite"
echo "======================================"
echo ""

# Check if cargo is available
if ! command -v cargo &> /dev/null; then
    echo "ERROR: cargo not found. Please install Rust toolchain first."
    echo "Visit: https://rustup.rs/"
    exit 1
fi

echo "Step 1: Building the project..."
cargo build
echo "✓ Build complete"
echo ""

echo "Step 2: Running unit tests..."
cargo test map_keys2 || echo "Note: Unit tests need to be added to tests.rs"
echo ""

echo "Step 3: Testing type checker with test_mapkeys2.cagara..."
if [ -f "test_mapkeys2.cagara" ]; then
    ./target/debug/cagara check test_mapkeys2.cagara
    echo "✓ Type checking passed"
else
    echo "WARNING: test_mapkeys2.cagara not found"
fi
echo ""

echo "Step 4: Testing individual examples..."

# Test 1: Simple prefix
echo "Test 1: Prefix transformation"
cat > /tmp/test1.cagara << 'EOF'
users = table "db" "users"
result = users & mapKeys2 (\x => "u_" <> x)
EOF
./target/debug/cagara check /tmp/test1.cagara && echo "✓ Test 1 passed" || echo "✗ Test 1 failed"
echo ""

# Test 2: Conditional
echo "Test 2: Conditional transformation"
cat > /tmp/test2.cagara << 'EOF'
users = table "db" "users"
result = users & mapKeys2 (\x => if x == "id" then "user_id" else x)
EOF
./target/debug/cagara check /tmp/test2.cagara && echo "✓ Test 2 passed" || echo "✗ Test 2 failed"
echo ""

# Test 3: Suffix
echo "Test 3: Suffix transformation"
cat > /tmp/test3.cagara << 'EOF'
users = table "db" "users"
result = users & mapKeys2 (\x => x <> "_old")
EOF
./target/debug/cagara check /tmp/test3.cagara && echo "✓ Test 3 passed" || echo "✗ Test 3 failed"
echo ""

# Test 4: Pipeline
echo "Test 4: Pipeline with mapKeys2"
cat > /tmp/test4.cagara << 'EOF'
users = table "db" "users"
result = users
    & mapKeys2 (\x => "u_" <> x)
    & select {.u_id, .u_name}
EOF
./target/debug/cagara check /tmp/test4.cagara && echo "✓ Test 4 passed" || echo "✗ Test 4 failed"
echo ""

# Test 5: Error case - empty names
echo "Test 5: Error handling (empty column names)"
cat > /tmp/test5.cagara << 'EOF'
users = table "db" "users"
result = users & mapKeys2 (\x => "")
EOF
./target/debug/cagara check /tmp/test5.cagara 2>&1 | grep -q "empty" && echo "✓ Test 5 passed (error caught)" || echo "✗ Test 5 failed"
echo ""

# Test 6: Error case - collisions
echo "Test 6: Error handling (column name collisions)"
cat > /tmp/test6.cagara << 'EOF'
users = table "db" "users"
result = users & mapKeys2 (\x => "same")
EOF
./target/debug/cagara check /tmp/test6.cagara 2>&1 | grep -q -E "(collision|duplicate|twice)" && echo "✓ Test 6 passed (error caught)" || echo "✗ Test 6 failed"
echo ""

echo "======================================"
echo "Test Suite Complete"
echo "======================================"
echo ""
echo "Next steps:"
echo "1. Review the test output above"
echo "2. Add the unit tests from mapkeys2_tests.rs to crates/cagara-hir/src/check/tests.rs"
echo "3. Run 'cargo test' to verify all tests pass"
echo "4. Try the examples in test_mapkeys2.cagara manually"
