pub fn value() -> u32 {
    1
}

pub trait PatternType: 'static {}

#[macro_export]
macro_rules! value {
    ($value:expr) => {
        $value
    };
}

#[inline]
pub fn positioned<T>() -> u32 {
    let _ = std::mem::size_of::<T>();
    1
}
