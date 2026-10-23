//! `fleet-check-rs`: the Rust port of `the original Python fleet-check`. The logic lives
//! in the library crate ([`fleet_check_rs::run`]); this binary only wires the
//! process streams to it.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    std::process::exit(fleet_check_rs::run(&args, &mut out, &mut err));
}
