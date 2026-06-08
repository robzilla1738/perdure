use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    exit(tach::cli::run(args));
}
