use ma_trace::covers;

#[covers("TS.TR1.CC.01")]
#[test]
fn annotated_test_still_runs() {
    assert_eq!(2 + 2, 4);
}

#[covers("A.01", "B.C.02")]
fn helper() -> u8 {
    7
}

#[test]
fn attribute_keeps_function_unchanged() {
    assert_eq!(helper(), 7);
}
