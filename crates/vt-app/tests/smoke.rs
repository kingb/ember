//! Integration smoke test: the binary builds, runs, exits 0, prints a banner.

#[test]
fn binary_prints_banner() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ember-term"))
        .output()
        .expect("failed to run ember-term");
    assert!(output.status.success(), "ember-term exited non-zero");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("ember-term"),
        "banner missing 'ember-term': {stdout}"
    );
}
