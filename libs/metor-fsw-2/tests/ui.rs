//! Compile-fail coverage for the fn-authoring surface: the
//! `#[diagnostic::on_unimplemented]` notes must keep pointing authors at the
//! accepted signatures. Regenerate expectations with `TRYBUILD=overwrite`
//! on toolchain bumps.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
}
