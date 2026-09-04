//! Windows only: the icon and version information for `attempt-hook.exe`.
//! See `crates/attempt/build.rs` — the same mark, so the two binaries read as
//! one product wherever Windows shows them.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=../../assets/icon/attemptdb.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/icon/attemptdb.ico")
            .set("ProductName", "AttemptDB")
            .set("FileDescription", "AttemptDB hook entrypoint")
            .set("CompanyName", "nullarch")
            .set("LegalCopyright", "Licensed under Apache-2.0")
            .set("OriginalFilename", "attempt-hook.exe");
        if let Err(e) = res.compile() {
            println!("cargo:warning=attempt-hook.exe was built without its icon: {e}");
        }
    }
}
