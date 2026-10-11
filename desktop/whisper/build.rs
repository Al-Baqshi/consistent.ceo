// On macOS, embed Info.plist as a __TEXT,__info_plist section so TCC reads the
// microphone usage-description string. Without it the OS SIGABRTs the process
// the instant cpal opens the input device. No-op on every other platform.
fn main() {
    #[cfg(target_os = "macos")]
    {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let plist = std::path::Path::new(&manifest).join("Info.plist");
        println!("cargo:rerun-if-changed=Info.plist");
        // -Wl,a,b,c,d passes a b c d as separate arguments to ld64, producing
        // `-sectcreate __TEXT __info_plist <path>`.
        println!(
            "cargo:rustc-link-arg=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
