//! Cargo build script for the gateway crate. Runs before compilation to stage
//! the dashboard frontend so `include_dir!` can embed it in the binary.
// When feature "dashboard" is enabled, copy dashboard build output into crate for include_dir!.
#[allow(unused_imports)]
use std::path::Path;

fn main() {
    #[cfg(feature = "dashboard")]
    {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let crate_root = Path::new(&manifest);
        // The SvelteKit frontend lives in `sveltfront/` and emits a static SPA
        // into `sveltfront/build` (adapter-static). Copy it in for include_dir!.
        let dashboard_build = crate_root.join("../../sveltfront/build");
        let out = crate_root.join("dashboard_build");
        if dashboard_build.is_dir() {
            if out.exists() {
                std::fs::remove_dir_all(&out).ok();
            }
            copy_dir_all(&dashboard_build, &out).expect("copy dashboard build");
            println!("cargo:rerun-if-changed=../../sveltfront/build");
        } else {
            // Feature requested but no build present: emit an empty dir so
            // include_dir! still compiles (serves 404s until the UI is built).
            std::fs::create_dir_all(&out).ok();
            println!(
                "cargo:warning=dashboard feature enabled but {} not found; run `npm run build` in sveltfront/ first",
                dashboard_build.display()
            );
        }
    }
}

// Recursive directory copy — std has no built-in for this.
#[cfg(feature = "dashboard")]
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let ty = e.file_type()?;
        let p = e.path();
        let name = p.file_name().unwrap();
        let dst_p = dst.join(name);
        if ty.is_dir() {
            copy_dir_all(&p, &dst_p)?;
        } else {
            std::fs::copy(&p, &dst_p)?;
        }
    }
    Ok(())
}
