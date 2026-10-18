fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Vendored protoc so builds need no system install.
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    println!("cargo:rerun-if-changed=../../proto/scion.proto");
    tonic_build::configure().compile_protos(&["../../proto/scion.proto"], &["../../proto"])?;
    Ok(())
}
