//@ needs-target-std
//@ ignore-cross-compile
//@ needs-asm-support

mod ledger;
mod model;
mod oracle;

use hegel::{HealthCheck, Hegel, Settings};

fn main() {
    oracle::witness_self_test();
    oracle::exercise_every_edit_kind();

    let settings = Settings::new().database(None).suppress_health_check([HealthCheck::TooSlow]);
    Hegel::new(oracle::run_generated).settings(settings).run();
}
