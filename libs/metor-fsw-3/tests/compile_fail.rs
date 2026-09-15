//! Compile-fail cases for `#[derive(Frame)]`.

#[test]
fn ui() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
