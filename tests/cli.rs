#[test]
fn disposable_cli_acceptance() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::process::Command::new("python3")
        .arg("tests/acceptance.py")
        .env("SPRIG_BINARY", env!("CARGO_BIN_EXE_sprig"))
        .output()?;
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn bulk_adoption_helper() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::process::Command::new("python3")
        .arg("tests/adoption.py")
        .env("SPRIG_BINARY", env!("CARGO_BIN_EXE_sprig"))
        .output()?;
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
