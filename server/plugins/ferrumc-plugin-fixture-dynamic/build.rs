#![forbid(unsafe_code)]

use std::{env, process};

fn main() {
    println!("cargo:rerun-if-env-changed=TARGET");

    let target = match env::var("TARGET") {
        Ok(target) => target,
        Err(error) => {
            eprintln!("Cargo did not provide a UTF-8 TARGET value: {error}");
            process::exit(1);
        }
    };

    println!("cargo:rustc-env=FERRUMC_FIXTURE_TARGET={target}");
}
