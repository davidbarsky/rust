extern crate a;

fn use_value() {
    let _: u32 = a::value();
}

#[cfg(second)]
fn diagnose_value() {
    a::value(1);
}
