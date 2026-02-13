// Bindings for monocp host functions when compiling for WASI preview2.
//
// We keep the legacy preview1 ABI (raw imports under module "monocp") for
// wasm32-wasip1 builds, and switch to component-model imports for wasm32-wasip2.

#![cfg(target_env = "p2")]

wit_bindgen::generate!({
    path: "wit",
    world: "monocp-world",
});
