fn main() {
    if let Err(err) = logfog::cli::run() {
        eprintln!("logfog: {err:#}");
        std::process::exit(1);
    }
}
