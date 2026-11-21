//! Validates a project's `.g4v/binaries.toml` the way g4v will read it:
//! `cargo run --example check_binaries_config -- <repository root>`
fn main() {
    let Some(root) = std::env::args().nth(1) else {
        eprintln!("usage: check_binaries_config <repository root>");
        std::process::exit(2);
    };
    match g4v::binaries::LoadedConfig::load(std::path::Path::new(&root)) {
        Ok(Some(c)) => println!("valid (hash {}):\n{:#?}", &c.hash[..12], c.config),
        Ok(None) => println!("no .g4v/binaries.toml in {root}"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
