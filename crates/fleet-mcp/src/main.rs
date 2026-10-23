//! `fleet-mcp`: MCP stdio server over the fleet registry check.
//!
//! Reads newline-delimited JSON-RPC 2.0 on stdin and answers on stdout; the
//! protocol and the tool live in the library crate.

fn main() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let mut runner = fleet_mcp::run_cli;
    fleet_mcp::serve(stdin.lock(), &mut out, &mut err, &mut runner);
}
