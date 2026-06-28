//! Integration smoke test: the binary builds, runs `--version`, exits 0, prints
//! a banner. (No-arg launches the GUI event loop, which needs a display, so the
//! headless smoke test uses `--version`.)

#[test]
fn binary_prints_banner() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ember-term"))
        .arg("--version")
        .output()
        .expect("failed to run ember-term");
    assert!(output.status.success(), "ember-term exited non-zero");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("ember-term"),
        "banner missing 'ember-term': {stdout}"
    );
}
