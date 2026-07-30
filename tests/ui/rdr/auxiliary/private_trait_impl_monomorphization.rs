//@ compile-flags: -Zrdr

#![crate_type = "rlib"]

trait Vector: Copy {
    const BYTES: usize;

    fn splat(byte: u8) -> Self;
}

impl Vector for u32 {
    const BYTES: usize = 4;

    fn splat(byte: u8) -> Self {
        u32::from(byte)
    }
}

struct Finder<V>(V);

impl<V: Vector> Finder<V> {
    #[inline(always)]
    fn new(byte: u8) -> Self {
        assert_eq!(V::BYTES, 4);
        Finder(V::splat(byte))
    }
}

#[inline]
pub fn find(byte: u8) -> u32 {
    Finder::<u32>::new(byte).0
}
