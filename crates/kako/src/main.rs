//! kako (加工): sandboxed data-transform runner.
//!
//! This is a skeleton. The run loop (take a job, stage inputs, run sandboxed,
//! hash outputs, report a receipt, clean up) is tracked in the repository
//! issues and described in `docs/decisions/0002-sandboxed-transform-runner.md`.

const USAGE: &str = "\
kako: sandboxed data-transform runner

USAGE:
    kako [--version | --help]

The run loop is not implemented yet. See docs/decisions/0002-sandboxed-transform-runner.md.";

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => println!("kako {}", env!("CARGO_PKG_VERSION")),
        Some("--help" | "-h") | None => println!("{USAGE}"),
        Some(other) => {
            eprintln!("kako: unknown argument `{other}`\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_set() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }
}
