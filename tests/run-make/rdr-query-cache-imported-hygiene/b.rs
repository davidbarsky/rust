extern crate a;

#[cfg(first)]
a::check_value!(ok);

#[cfg(second)]
a::check_value!(bad);
