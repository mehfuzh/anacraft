//! Cargo does not track the env vars read by `option_env!`, so a rebuild after
//! changing the baked-in OAuth client would silently reuse the previous values.
//! Declaring them here keeps a local `cargo build` honest.

fn main() {
    println!("cargo:rerun-if-env-changed=ANACRAFT_OAUTH_CLIENT_ID");
    println!("cargo:rerun-if-env-changed=ANACRAFT_OAUTH_CLIENT_SECRET");
}
