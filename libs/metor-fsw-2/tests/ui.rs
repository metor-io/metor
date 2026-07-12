//! Compile-fail coverage for the fn-authoring surface: the
//! `#[diagnostic::on_unimplemented]` notes must keep pointing authors at the
//! accepted signatures. Regenerate expectations with `TRYBUILD=overwrite`
//! on toolchain bumps. `pass/` holds must-compile cases (bound-sensitive
//! macro expansions such as the params-defaults probe).

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
    t.pass("tests/ui/pass/*.rs");
}
