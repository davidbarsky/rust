// First source layout.
#[macro_export]
macro_rules! check_value {
    (ok) => {
        const _: u32 = 1;
    };
    (bad) => {
        const _: u32 = "wrong";
    };
}
