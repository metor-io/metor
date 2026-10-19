//! Compile-fail cases for the derives.

#[test]
fn test_compile_errors() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/*.rs");
}
