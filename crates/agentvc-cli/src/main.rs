fn main() {
    println!(
        "agentvc {} (format v{})",
        env!("CARGO_PKG_VERSION"),
        agentvc_core::FORMAT_VERSION
    );
}
