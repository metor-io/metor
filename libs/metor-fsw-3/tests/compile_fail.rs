//! Compile-fail and pass cases for the derives and `#[system]`.

#[test]
fn ui() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/*.rs");
    cases.pass("tests/ui/pass/*.rs");
}
