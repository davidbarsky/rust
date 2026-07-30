#![allow(dead_code)]

struct Cache(u32);

unsafe impl Sync for Cache {}

static CACHED_VALUE: Cache = Cache(1);

fn private_value() -> u32 {
    fn nested() -> u32 {
        2
    }

    nested()
}

pub fn value() -> u32 {
    CACHED_VALUE.0
}

pub struct ProjectRoot;

impl ProjectRoot {
    pub fn write_file(&self, first: impl AsRef<str>, second: impl AsRef<[u8]>) {
        let _ = (first, second);
    }
}
