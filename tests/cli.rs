use std::process::Command;

#[test]
fn requires_config_flag() {
    let out = Command::new(env!("CARGO_BIN_EXE_evm-contract-exporter"))
        .output()
        .expect("run");
    assert!(!out.status.success());
}

#[test]
fn rejects_bad_log_format() {
    // A present-but-unparseable config path still fails fast on log format.
    let out = Command::new(env!("CARGO_BIN_EXE_evm-contract-exporter"))
        .args(["--config", "/nonexistent.yaml", "--log-format", "xml"])
        .output()
        .expect("run");
    assert!(!out.status.success());
}
