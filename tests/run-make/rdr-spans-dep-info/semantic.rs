extern crate a;

pub fn value() -> u32 {
    a::value!(a::value())
}

pub async fn parse_patterns<T: a::PatternType>() {}
