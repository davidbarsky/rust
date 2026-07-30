#![allow(dead_code)]

fn private_names() {
    let apricot = 0;
    let blackberry = 0;
    let cantaloupe = 0;
    let dragonfruit = 0;
    let elderberry = 0;
    let feijoa = 0;
    let grapefruit = 0;
    let honeydew = 0;
    let _ =
        (apricot, blackberry, cantaloupe, dragonfruit, elderberry, feijoa, grapefruit, honeydew);
}

/// [`Alpha`] [`Beta`] [`Gamma`] [`Delta`] [`Epsilon`] [`Zeta`] [`Eta`] [`Theta`]
pub struct Api;

pub struct Alpha;
pub struct Beta;
pub struct Gamma;
pub struct Delta;
pub struct Epsilon;
pub struct Zeta;
pub struct Eta;
pub struct Theta;
