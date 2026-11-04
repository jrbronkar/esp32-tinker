#[test]
fn discover_help_exposes_mdns_interface_not_private_probe_options() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_espctl"))
        .args(["discover", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("--interface"));
    assert!(!text.contains("--broadcast"));
    assert!(!text.contains("--port"));
}
