//! Raw FFI bindings to `moonlight-common-c`'s public API (`Limelight.h`), generated at build
//! time by `build.rs`. This crate is intentionally just the raw bindings — no safe wrapper here;
//! that lives in `rdclient` alongside the code that already owns decode (`decode.rs`) and
//! presentation/input (`input_surface.rs`), since the safe API shape depends on how those hook in.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
