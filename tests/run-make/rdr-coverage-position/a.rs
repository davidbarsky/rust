#[cfg(first)]
#[inline(always)]
pub fn generic<T>(value: T) -> T {
    if std::hint::black_box(true) { value } else { unreachable!() }
}

#[cfg(second)]
#[inline(always)]
pub fn generic<T>(value: T) -> T {
    if std::hint::black_box(true) { value } else { unreachable!() }
}
