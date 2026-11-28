//! Reusable command orchestration. Engine SQL stays in the engine crates.

pub mod resolver;

pub mod emission;

#[cfg(test)]
mod certified_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../pbps-diff/tests/support/emission.rs"
    ));
}

#[cfg(test)]
mod final_order_test;
#[cfg(test)]
use final_order_test::final_order_refusal;
