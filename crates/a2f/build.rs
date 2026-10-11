//! Build script: locates a vendored `protoc` if none is on PATH, then
//! compiles `proto/audio2face.proto` into Rust types via tonic-build.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("PROTOC").is_none()
        && let Ok(path) = protoc_bin_vendored::protoc_bin_path()
    {
        // SAFETY: build.rs runs single-threaded before compilation.
        unsafe {
            std::env::set_var("PROTOC", path);
        }
    }

    // Client-only: this crate only consumes the Audio2Face service, so skip
    // generating server code.
    tonic_build::configure()
        .build_server(false)
        .compile_protos(&["proto/audio2face.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/audio2face.proto");
    Ok(())
}
