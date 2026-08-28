use wasm_bindgen::prelude::*;
pub use wasm-bindgen-rayon::init_thread_pool;

#[wasm_bindgen]
pub fn greet() -> String {
    "Hello from FerrumC!".to_string()
}
