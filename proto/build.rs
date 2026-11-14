fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "vmlayer/v1/common.proto",
        "vmlayer/v1/sandboxes.proto",
        "vmlayer/v1/processes.proto",
        "vmlayer/v1/files.proto",
        "vmlayer/v1/volumes.proto",
        "vmlayer/v1/system.proto",
    ];
    for p in &protos {
        println!("cargo:rerun-if-changed={p}");
    }
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&protos, &["."])?;
    Ok(())
}
