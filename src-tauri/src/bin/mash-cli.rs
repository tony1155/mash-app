// mash-cli: headless automation entry. See src/cli.rs for the interface
// contract and docs/02-mash-cli-spec.md in the integration repo.

fn main() {
    let code = mash_lib::cli::cli_main();
    // Give stdout a chance to flush before the process exits.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
