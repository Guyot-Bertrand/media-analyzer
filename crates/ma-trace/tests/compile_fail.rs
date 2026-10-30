#[test]
fn rejects_invalid_or_missing_ids() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
