//@ run-pass
//@ ignore-cross-compile
//@ ignore-remote

#![feature(rustc_private)]

extern crate rustc_data_structures;
extern crate rustc_driver;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_span;

use rustc_data_structures::stable_hash::{
    SpanHashMode, StableHashControls, StableHashCtxt,
};
use rustc_driver::Compilation;
use rustc_interface::interface::{self, Compiler};
use rustc_middle::ty::TyCtxt;

struct CheckSpanHashModeRestoration;

impl rustc_driver::Callbacks for CheckSpanHashModeRestoration {
    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        tcx.with_stable_hashing_context(|mut hcx| {
            assert_eq!(
                hcx.stable_hash_controls(),
                StableHashControls::Span(SpanHashMode::Full)
            );

            hcx.with_span_hash_mode(SpanHashMode::Hygiene, |hcx| {
                assert_eq!(
                    hcx.stable_hash_controls(),
                    StableHashControls::Span(SpanHashMode::Hygiene)
                );

                hcx.with_span_hash_mode(SpanHashMode::Ignore, |hcx| {
                    assert_eq!(
                        hcx.stable_hash_controls(),
                        StableHashControls::Span(SpanHashMode::Ignore)
                    );
                });

                assert_eq!(
                    hcx.stable_hash_controls(),
                    StableHashControls::Span(SpanHashMode::Hygiene)
                );
            });

            assert_eq!(
                hcx.stable_hash_controls(),
                StableHashControls::Span(SpanHashMode::Full)
            );

            hcx.with_span_hash_mode(SpanHashMode::Ignore, |hcx| {
                hcx.with_span_hash_mode(SpanHashMode::Hygiene, |hcx| {
                    assert_eq!(
                        hcx.stable_hash_controls(),
                        StableHashControls::Span(SpanHashMode::Ignore)
                    );
                });
            });

            let result = rustc_driver::catch_fatal_errors(|| {
                hcx.with_span_hash_mode(SpanHashMode::Ignore, |hcx| {
                    assert_eq!(
                        hcx.stable_hash_controls(),
                        StableHashControls::Span(SpanHashMode::Ignore)
                    );
                    rustc_span::fatal_error::FatalError.raise();
                });
            });

            assert!(result.is_err());
            assert_eq!(
                hcx.stable_hash_controls(),
                StableHashControls::Span(SpanHashMode::Full)
            );
        });

        Compilation::Stop
    }
}

fn main() {
    let input = "span_hash_mode_restoration_input.rs";
    std::fs::write(input, "#![feature(no_core)]\n#![no_core]").unwrap();

    let args = ["rustc".to_string(), "--crate-type=lib".to_string(), input.to_string()];
    rustc_driver::catch_fatal_errors(|| -> interface::Result<()> {
        rustc_driver::run_compiler(&args, &mut CheckSpanHashModeRestoration);
        Ok(())
    })
    .unwrap()
    .unwrap();
}
