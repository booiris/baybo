//! Drift check: `docs/openapi.json` must match what the gateway emits.
//!
//! Rendering the admin `OpenApi` document is pure — no I/O, no state —
//! so the test just assembles it via [`v1_router_and_spec`] and
//! byte-compares against the checked-in spec.
//!
//! When the shape of the admin API changes on purpose, regenerate:
//!
//! ```bash
//! UPDATE_OPENAPI=1 cargo test -p baybo-gateway --test all openapi_json_is_in_sync
//! ```
//!
//! Commit the resulting `docs/openapi.json` in the same change that
//! touches the handlers / DTOs. CI runs this test without the env var
//! and will fail if the two drift apart.

use std::path::PathBuf;

use baybo_gateway::api::admin::v1_router_and_spec;

fn spec_path() -> PathBuf {
    // CARGO_MANIFEST_DIR is the gateway crate; the repo root is two
    // levels up (crates/gateway -> crates -> repo root).
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("docs");
    p.push("openapi.json");
    p
}

fn rendered_spec() -> String {
    let (_router, spec) = v1_router_and_spec();
    let mut s = serde_json::to_string_pretty(&spec).expect("serialize openapi");
    s.push('\n');
    s
}

#[test]
fn openapi_json_is_in_sync() {
    let path = spec_path();
    let rendered = rendered_spec();

    if std::env::var_os("UPDATE_OPENAPI").is_some() {
        std::fs::write(&path, &rendered).expect("write docs/openapi.json");
        return;
    }

    let on_disk = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "failed to read {}: {e}. run `UPDATE_OPENAPI=1 cargo test -p baybo-gateway --test all openapi_json_is_in_sync` to create it",
            path.display()
        )
    });

    if on_disk != rendered {
        panic!(
            "docs/openapi.json is out of sync with the gateway's OpenAPI spec.\n\
             run `UPDATE_OPENAPI=1 cargo test -p baybo-gateway --test all openapi_json_is_in_sync` to regenerate, \
             then commit the updated file."
        );
    }
}
