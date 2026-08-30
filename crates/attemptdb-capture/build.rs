// Expose the compile target so `attempt update` can name the right release
// asset (`attempt-<version>-<target>.tar.gz`) without guessing at runtime.
fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=ATTEMPTDB_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
