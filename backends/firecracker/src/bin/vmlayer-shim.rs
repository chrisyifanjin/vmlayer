//! One helper process per microVM; see `vmlayer_backend_firecracker::shim`.
fn main() {
    std::process::exit(vmlayer_backend_firecracker::shim::main());
}
