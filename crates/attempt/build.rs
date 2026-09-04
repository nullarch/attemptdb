//! Windows only: give `attempt.exe` its icon and its version information.
//!
//! Explorer, the taskbar, Alt-Tab and the SmartScreen dialog all read these.
//! Everywhere else this is a no-op — a CLI needs no icon on macOS or Linux.
//! A missing resource compiler is a warning, never a failed build: an
//! executable without an icon still works.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=../../assets/icon/attemptdb.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/icon/attemptdb.ico")
            .set("ProductName", "AttemptDB")
            .set(
                "FileDescription",
                "AttemptDB — the database for what AI coding agents tried",
            )
            .set("CompanyName", "nullarch")
            .set("LegalCopyright", "Licensed under Apache-2.0")
            .set("OriginalFilename", "attempt.exe");
        if let Err(e) = res.compile() {
            println!("cargo:warning=attempt.exe was built without its icon: {e}");
        }
    }
}
