//! `Secret` wipes its memory when it is dropped, and the type system says so.
#![allow(clippy::unwrap_used)]

use pkgproxy::Secret;

#[test]
fn a_secret_wipes_its_memory_on_drop() {
    fn assert_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}
    assert_zeroize_on_drop::<Secret>();
}
