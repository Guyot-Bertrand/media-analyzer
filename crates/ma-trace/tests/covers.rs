use ma_trace::covers;

#[covers("TOOL.TRACE.01")]
#[test]
fn annotated_test_still_runs() {
    assert_eq!(2 + 2, 4);
}

#[covers("TOOL.TRACE.01", "TOOL.TRACE.02")]
fn helper() -> u8 {
    7
}

#[covers("TOOL.TRACE.01")]
#[test]
fn attribute_keeps_function_unchanged() {
    assert_eq!(helper(), 7);
}
