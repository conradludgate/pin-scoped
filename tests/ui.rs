#![cfg(not(any(miri, pin_scoped_loom)))]

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
}
