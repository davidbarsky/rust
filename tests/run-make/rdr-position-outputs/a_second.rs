use std::panic::Location;

// The second revision moves the unchanged exported items.
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
//
#[inline(always)]
pub fn generic<T>(value: T) -> T {
    if std::hint::black_box(true) { value } else { unreachable!() }
}

//
#[track_caller]
pub fn tracked() -> &'static Location<'static> {
    Location::caller()
}

//
#[macro_export]
macro_rules! define_imported {
    () => {
        pub fn imported(value: u32) -> u32 {
            if std::hint::black_box(true) { value } else { unreachable!() }
        }
    };
}
