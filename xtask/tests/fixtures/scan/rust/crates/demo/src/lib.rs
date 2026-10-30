pub fn add(a: u8, b: u8) -> u8 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[ma_trace::covers("DEMO.ADD.01")]
    #[test]
    fn adds() {
        assert_eq!(add(1, 2), 3);
    }

    #[test]
    fn not_linked() {}
}

#[ma_trace::covers("DEMO.MOD.01")]
mod misplaced {}

#[ma_trace::covers("DEMO.HELP.01")]
fn helper_without_test() {}
