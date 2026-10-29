#[test]
fn disposable_cli_acceptance() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::process::Command::new("python3")
        .arg("tests/acceptance.py")
        .env("TOOL_BINARY", env!("CARGO_BIN_EXE_tool"))
        .output()?;
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
