#![forbid(unsafe_code)]

fn main() {
    println!("cargo:rerun-if-env-changed=TARGET");
    let target = match std::env::var("TARGET") {
        Ok(target) => target,
        Err(error) => {
            eprintln!("failed to read Cargo TARGET for FerrumC plugin metadata: {error}");
            std::process::exit(1);
        }
    };
    println!("cargo:rustc-env=FERRUMC_PLUGIN_TARGET={target}");
}
