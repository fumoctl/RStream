//! Stream target scoping by manifest resource, media type, ID prefix, and addon URL.
mod common;
use common::Data;
use serde_json::{json, Value};

fn stream_addon(name: &str, types: &[&str], prefixes: &[&str]) -> Value {
    json!({
        "id": format!("test.{name}"),
        "name": name,
        "version": "1.0.0",
        "types": types,
        "idPrefixes": prefixes,
        "resources": ["stream"],
        "catalogs": []
    })
}

#[tokio::test]
async fn streams_targets_respects_type_prefix_and_optional_addon_url() {
    let data = Data::new();
    drop(data.core());
    let a = "https://addon-a.example/manifest.json";
    let b = "https://addon-b.example/manifest.json";
    data.install(a, &stream_addon("Addon A", &["movie", "series"], &["tt"]));
    data.install(b, &stream_addon("Addon B", &["anime"], &["kitsu:"]));
    let core = data.core();

    let imdb_targets = core
        .execute(json!({"op":"streams_targets","type":"movie","id":"tt0133093"}))
        .await
        .unwrap();
    assert_eq!(
        imdb_targets["targets"],
        json!([{"addon_name": "Addon A", "addon_url": a}])
    );

    let anime_targets = core
        .execute(json!({"op":"streams_targets","type":"anime","id":"kitsu:123:1"}))
        .await
        .unwrap();
    assert_eq!(
        anime_targets["targets"],
        json!([{"addon_name": "Addon B", "addon_url": b}])
    );

    let scoped = core
        .execute(json!({"op":"streams_targets","type":"movie","id":"tt0133093","addon_url":b}))
        .await
        .unwrap();
    assert_eq!(scoped["targets"], json!([]));
}
