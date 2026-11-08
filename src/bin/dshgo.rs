//! `dshgo`: short alias of `go-dsh`.
fn main() {
    let code = go_dsh::cli::main_with_args(std::env::args().collect());
    std::process::exit(code);
}
