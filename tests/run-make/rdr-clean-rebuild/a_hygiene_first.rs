#![allow(dead_code)]

fn private_value() -> &'static str {
    "private"
}

macro_rules! define_api {
    () => {
        pub fn public_value() -> u32 {
            1
        }
    };
}

define_api!();
