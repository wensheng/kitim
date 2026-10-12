// Rust guideline compliant 2026-10-07
use std::process::Command;

#[test]
fn built_executable_starts_with_native_runtime() {
    // Starting the linked binary also exercises Windows DLL discovery before main.
    let output = Command::new(env!("CARGO_BIN_EXE_kitim"))
        .arg("--help")
        .output()
        .expect("kitim should launch");
    assert!(
        output.status.success(),
        "kitim exited with {}",
        output.status
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
}
