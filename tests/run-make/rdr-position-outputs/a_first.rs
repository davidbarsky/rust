use std::panic::Location;

// First source layout.
#[inline(always)]
pub fn generic<T>(value: T) -> T {
    if std::hint::black_box(true) { value } else { unreachable!() }
}

// First source layout.
#[track_caller]
pub fn tracked() -> &'static Location<'static> {
    Location::caller()
}

// First source layout.
#[macro_export]
macro_rules! define_imported {
    () => {
        pub fn imported(value: u32) -> u32 {
            if std::hint::black_box(true) { value } else { unreachable!() }
        }
    };
}
