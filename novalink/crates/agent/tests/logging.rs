#[test]
fn init_writes_json_lines() {
    let dir = tempfile::tempdir().unwrap();
    let guard = novalink_agent::logging::init(dir.path()).unwrap();
    tracing::info!(k = 1, "hello-from-test");
    drop(guard); // flush
    let mut found = false;
    for e in std::fs::read_dir(dir.path()).unwrap() {
        let s = std::fs::read_to_string(e.unwrap().path()).unwrap();
        for line in s.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            if v["fields"]["message"] == "hello-from-test" {
                found = true;
            }
        }
    }
    assert!(found);
}
