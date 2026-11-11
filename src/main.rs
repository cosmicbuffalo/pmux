/// Entry point for the pmux binary; delegates to the CLI dispatcher.
fn main() -> anyhow::Result<()> {
    pmux::cli_main()
}
