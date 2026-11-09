//! Public configuration compatibility retained by the fork.

use mcpls_core::config::LspServerConfig;
use serde_json::{Value, json};

#[test]
fn initialization_options_keep_the_json_api_and_redacted_parent_debug() {
    let mut server = LspServerConfig::rust_analyzer();
    let options = json!({"ordinary": "private-value", "nested": [true, 7]});
    let public_field: &mut Option<Value> = &mut server.initialization_options;
    *public_field = Some(options.clone());

    assert!(!format!("{server:?}").contains("private-value"));
    assert_eq!(
        serde_json::to_value(&server).unwrap()["initialization_options"],
        options
    );
}

#[test]
fn config_boundary_rejects_nested_datetime_but_transmits_date_strings() {
    let base = "language_id = 'python'\ncommand = '/usr/bin/true'\n";
    for options in [
        "{ nested = { when = 2026-11-09 } }",
        "{ values = [1, 2026-11-09T12:34:56Z] }",
    ] {
        assert!(
            toml::from_str::<LspServerConfig>(&format!(
                "{base}initialization_options = {options}\n"
            ))
            .is_err()
        );
    }
    let server: LspServerConfig = toml::from_str(&format!(
        "{base}initialization_options = {{ nested = {{ when = '2026-11-09' }} }}\n"
    ))
    .unwrap();
    assert_eq!(
        server.initialization_options,
        Some(json!({"nested": {"when": "2026-11-09"}}))
    );
}
