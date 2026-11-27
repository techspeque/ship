//! Fuzzes `ship_source::unpack` through the shared harness, which panics
//! if a byte lands outside the staging directory, a failure leaves the
//! staging directory behind, or a success leaves anything but files and
//! directories.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    ship_source::fuzzing::check_unpack_in_temp(data);
});
