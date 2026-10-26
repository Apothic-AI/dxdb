// napi-rs link setup for the addon build.
//
// `cfg(feature = ...)` is *not* available in a build script — those cfgs apply to
// the crate's own compilation, not to `build.rs`. Cargo exposes enabled features to
// build scripts as `CARGO_FEATURE_<NAME>` env vars (uppercased, `-` becomes `_`),
// which is what this reads.
//
// Without the feature this is a no-op, so `make check`/`test`/`clippy`/`wasm` do
// not require Node's addon link flags.
fn main() {
    if std::env::var_os("CARGO_FEATURE_NODE_ADDON").is_some() {
        napi_build::setup();
    }
}
