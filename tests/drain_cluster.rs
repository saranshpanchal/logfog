use logfog::Engine;

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn drain_collapses_parameterized_lines() {
    let mut engine = Engine::new();
    engine.ingest_path(&fixture("drain.log"), false).unwrap();
    let rows = engine.patterns("", "count").unwrap();
    let templates: Vec<&str> = rows.iter().map(|r| r.template.as_str()).collect();
    assert!(
        templates.contains(&"User <*> logged in from <*>"),
        "{templates:?}"
    );
    assert!(
        templates.contains(&"Failed password for <*> from <*>"),
        "{templates:?}"
    );
    let user = rows
        .iter()
        .find(|r| r.template == "User <*> logged in from <*>")
        .unwrap();
    assert_eq!(user.count, 3);
    assert!(!user.spark.is_empty());
}
