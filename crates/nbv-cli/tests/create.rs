use std::process::Command;
#[test]
fn create_exits_without_neovim_and_refuses_collisions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.ipynb");
    let create = || {
        Command::new(env!("CARGO_BIN_EXE_nvb"))
            .env("NBV_NVIM", "/nvb-missing-neovim")
            .arg("--create")
            .arg(&path)
            .output()
            .unwrap()
    };
    let output = create();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let bytes = std::fs::read(&path).unwrap();
    let notebook: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(notebook["cells"], serde_json::json!([]));
    assert_eq!(notebook["metadata"]["kernelspec"]["language"], "python");
    let collision = create();
    assert!(!collision.status.success());
    assert!(String::from_utf8_lossy(&collision.stderr).contains("cannot create"));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}
