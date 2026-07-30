use std::sync::atomic::{AtomicU32, Ordering};

static VALUE: AtomicU32 = AtomicU32::new(0);

pub fn next_value() -> u32 {
    VALUE.fetch_add(1, Ordering::SeqCst) + 1
}
