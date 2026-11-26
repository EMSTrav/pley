//! Mixes (docs/API.md: Mixes): made from a few artists and the artists like
//! them, by Pley or by a stand-in for an OpenAI-compatible AI service, and
//! kept as playlists.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use parking_lot::Mutex;
use serde_json::{Value, json};

use common::TestApp;

/// What the stand-in AI service was asked: the Authorization it got, and the body.
type Asked = Arc<Mutex<Vec<(Option<String>, Value)>>>;

/// The stand-in AI service: answers with `reply` (the listed tracks, last
/// first, and one it wasn't given) and keeps what it was asked.
async fn ai_service(fail: bool) -> (Asked, String) {
    let asked: Asked = Arc::default();
    let seen = asked.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, axum::Json(body): axum::Json<Value>| {
            let seen = seen.clone();
            async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_owned);
                seen.lock().push((auth, body.clone()));
                if fail {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(json!({"error": {"message": "overloaded"}})),
                    );
                }
                let prompt = body["messages"][1]["content"].as_str().unwrap_or_default();
                let mut ids: Vec<i64> =
                    prompt.lines().filter_map(|l| l.split(" | ").next()?.trim().parse().ok()).collect();
                ids.reverse();
                ids.push(999_999);
                let content = json!({"title": "Night Drive", "tracks": ids}).to_string();
                (
                    StatusCode::OK,
                    axum::Json(json!({"choices": [{"message": {"role": "assistant", "content": content}}]})),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (asked, format!("http://{addr}/v1"))
}

/// A music library of four artists, three of them house, with their ids.
async fn music(app: &TestApp, admin: &str) -> (i64, std::collections::HashMap<String, i64>) {
    for (artist, album, tracks) in
        [("Band", "Record", 4), ("Another", "First", 3), ("Third", "Thing", 2), ("Jazzer", "Blue", 2)]
    {
        for n in 1..=tracks {
            app.media_file(&format!("music/{artist}/{album}/0{n} - {artist} {n}.mp3"), 512);
        }
    }
    let lib = app.library(admin, "Music", "music", "music").await;
    let ids = app.ids(lib, admin).await;
    for (artist, genres) in
        [("Band", &["House"][..]), ("Another", &["House", "Electronic"]), ("Third", &["house"]), ("Jazzer", &["Jazz"])]
    {
        for genre in genres {
            sqlx::query(
                "INSERT OR IGNORE INTO item_genres (genre, library_id, kind, item_id) VALUES (?, ?, 'artist', ?)",
            )
            .bind(genre)
            .bind(lib)
            .bind(ids[artist])
            .execute(&app.state.db.write)
            .await
            .unwrap();
        }
    }
    (lib, ids)
}

/// The artists of a playlist's tracks, in order.
async fn artists_of(app: &TestApp, token: &str, playlist: &Value) -> Vec<String> {
    let detail = app.get(&format!("/api/playlists/{}", playlist["playlist_id"]), token).await;
    assert_eq!(detail.status, StatusCode::OK, "{:?}", detail.body);
    detail.body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["item"]["grandparent_title"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn a_mix_without_ai_plays_the_artists_and_those_like_them() {
    let app = TestApp::new().await;
    let admin = app.setup_admin().await;
    let (_, ids) = music(&app, &admin).await;

    let info = app.get("/api/mixes", &admin).await;
    assert_eq!(info.body["ai"], false, "{:?}", info.body);

    let made = app.post("/api/mixes", &admin, json!({"artist_ids": [ids["Band"]], "length": 50})).await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    assert_eq!((made.body["title"].as_str(), made.body["by_ai"].as_bool()), (Some("Band Mix"), Some(false)));
    let artists = artists_of(&app, &admin, &made.body).await;
    assert_eq!(artists.len(), 9, "every track of the house artists: {artists:?}");
    assert_eq!(made.body["item_count"], 9);
    assert!(!artists.contains(&"Jazzer".to_owned()), "no artist with nothing in common: {artists:?}");
    assert_eq!(artists.iter().filter(|a| *a == "Band").count(), 4);
    let playlists = app.get("/api/playlists", &admin).await;
    assert_eq!(playlists.body["playlists"][0]["kind"], "audio", "kept as a music playlist");

    let short = app.post("/api/mixes", &admin, json!({"artist_ids": [ids["Band"], ids["Jazzer"]], "length": 3})).await;
    assert_eq!(short.body["item_count"], 3);
    assert_eq!(short.body["title"], "Band, Jazzer Mix");

    // What can't make a mix.
    let many: Vec<i64> = (0..6).map(|_| ids["Band"]).collect();
    for (body, status) in [
        (json!({"artist_ids": []}), StatusCode::BAD_REQUEST),
        (json!({"artist_ids": many}), StatusCode::BAD_REQUEST),
        (json!({"artist_ids": [ids["Band"]], "length": 0}), StatusCode::BAD_REQUEST),
        (json!({"artist_ids": [ids["Band"]], "length": 101}), StatusCode::BAD_REQUEST),
        (json!({"artist_ids": [ids["Band"]], "prompt": "x".repeat(201)}), StatusCode::BAD_REQUEST),
        (json!({"artist_ids": [987_654]}), StatusCode::NOT_FOUND),
    ] {
        let refused = app.post("/api/mixes", &admin, body.clone()).await;
        assert_eq!(refused.status, status, "{body}: {:?}", refused.body);
    }
    // Nor an artist of a library someone may not open.
    let (kid, _) = app.user(&admin, "kid", json!({"all_libraries": false, "library_ids": []})).await;
    let theirs = app.post("/api/mixes", &kid, json!({"artist_ids": [ids["Band"]]})).await;
    assert_eq!(theirs.status, StatusCode::NOT_FOUND, "{:?}", theirs.body);
}

#[tokio::test]
async fn an_ai_service_chooses_a_mixs_tracks_from_the_library() {
    let app = TestApp::new().await;
    let admin = app.setup_admin().await;
    let (_, ids) = music(&app, &admin).await;
    let (asked, endpoint) = ai_service(false).await;

    // On only with an address and a model.
    let half = app.put("/api/settings", &admin, json!({"ai_mixes": true})).await;
    assert_eq!(half.status, StatusCode::BAD_REQUEST);
    let bad = app.put("/api/settings", &admin, json!({"ai_endpoint": "ftp://ai.example"})).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let settings = json!({"ai_mixes": true, "ai_endpoint": format!("{endpoint}/"), "ai_model": "test-model", "ai_api_key": "ai-key"});
    let saved = app.put("/api/settings", &admin, settings).await;
    assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
    assert_eq!(app.get("/api/mixes", &admin).await.body["ai"], true);

    let tested = app.post("/api/settings/test", &admin, json!({"service": "ai"})).await;
    assert_eq!(tested.body["ok"], true, "{:?}", tested.body);

    let made = app
        .post(
            "/api/mixes",
            &admin,
            json!({"artist_ids": [ids["Another"]], "length": 20, "prompt": "for a \"long\" drive"}),
        )
        .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    assert_eq!((made.body["title"].as_str(), made.body["by_ai"].as_bool()), (Some("Night Drive"), Some(true)));
    assert_eq!(made.body["item_count"], 9, "the tracks it was given, not the one it made up");
    let artists = artists_of(&app, &admin, &made.body).await;
    assert!(!artists.contains(&"Jazzer".to_owned()), "{artists:?}");

    let (auth, body) = asked.lock().last().cloned().unwrap();
    assert_eq!(auth.as_deref(), Some("Bearer ai-key"));
    assert_eq!(body["model"], "test-model");
    let prompt = body["messages"][1]["content"].as_str().unwrap();
    assert!(prompt.contains("starts from these artists: Another"), "{prompt}");
    assert!(prompt.contains("is for: \"for a 'long' drive\""), "{prompt}");
    assert!(!prompt.contains("music/") && !prompt.contains(".mp3"), "no file is named: {prompt}");

    // A test of another address doesn't send it the saved key.
    let elsewhere =
        app.post("/api/settings/test", &admin, json!({"service": "ai", "endpoint": format!("{endpoint}/../v1")})).await;
    assert_eq!(elsewhere.status, StatusCode::OK);
    assert_eq!(asked.lock().last().unwrap().0, None, "{:?}", elsewhere.body);
}

#[tokio::test]
async fn pley_makes_the_mix_when_the_ai_service_fails() {
    let app = TestApp::new().await;
    let admin = app.setup_admin().await;
    let (_, ids) = music(&app, &admin).await;
    let (asked, endpoint) = ai_service(true).await;
    let settings = json!({"ai_mixes": true, "ai_endpoint": endpoint, "ai_model": "test-model"});
    assert_eq!(app.put("/api/settings", &admin, settings).await.status, StatusCode::OK);

    let made = app.post("/api/mixes", &admin, json!({"artist_ids": [ids["Third"]]})).await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    assert_eq!((made.body["title"].as_str(), made.body["by_ai"].as_bool()), (Some("Third Mix"), Some(false)));
    assert_eq!(made.body["item_count"], 9);
    assert_eq!(asked.lock().len(), 1);
    assert_eq!(asked.lock()[0].0, None, "no key, no Authorization");

    let tested = app.post("/api/settings/test", &admin, json!({"service": "ai"})).await;
    assert_eq!(tested.body["ok"], false);
    assert!(tested.body["message"].as_str().unwrap().contains("overloaded"), "{:?}", tested.body);
}
