struct S;
impl S {
    #[ma_trace::covers("A.B.01")]
    fn m(&self) {}
}
#[ma_trace::covers("A.B.02")]
impl Clone for S { fn clone(&self) -> S { S } }
fn main() { S.m() }
