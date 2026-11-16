//! Audiobook libraries: books found in folders and `.m4b` files, their
//! parts, where Play takes up a book, and Continue Listening.

mod common;

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};

use common::{TestApp, titles};

/// A library of two books: one in a folder of two parts, in its author's
/// series, and one `.m4b` file.
async fn books() -> (TestApp, String, i64) {
    let app = TestApp::new().await;
    let token = app.setup_admin().await;
    let book = "books/Author A/Series X/Book 1 - The Long Road (2006)";
    app.media_file(&format!("{book}/01 - Prologue.mp3"), 512);
    app.media_file(&format!("{book}/02 - Chapter One.mp3"), 512);
    app.media_file("books/Author B/Middle Book.m4b", 512);
    let lib = app.library(&token, "Audiobooks", "audiobooks", "books").await;
    (app, token, lib)
}

async fn progress(app: &TestApp, token: &str, part: i64, position_ms: i64) {
    let r = app
        .post(
            &format!("/api/items/{part}/progress"),
            token,
            json!({"position_ms": position_ms, "duration_ms": 3_600_000, "state": "stopped"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{:?}", r.body);
}

async fn on_deck(app: &TestApp, token: &str, book: i64) -> Value {
    let deck = app.get(&format!("/api/items/{book}/on-deck"), token).await;
    assert_eq!(deck.status, StatusCode::OK);
    deck.body["item"].clone()
}

async fn hub(app: &TestApp, token: &str, uri: &str, key: &str) -> Vec<String> {
    let hubs = app.get(uri, token).await;
    assert_eq!(hubs.status, StatusCode::OK, "{:?}", hubs.body);
    hubs.body["hubs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["key"].as_str().is_some_and(|k| k.starts_with(key)))
        .map(|h| titles(&h["items"]))
        .unwrap_or_default()
}

/// Books are the library's titles, with their writers; a book's page says
/// who wrote it and the series it's in, and lists its parts in order.
#[tokio::test]
async fn books_and_their_parts() {
    let (app, token, lib) = books().await;
    let page = app.get(&format!("/api/libraries/{lib}/items"), &token).await;
    assert_eq!(page.status, StatusCode::OK, "{:?}", page.body);
    let items = page.body["items"].as_array().unwrap();
    // By title, "The" left out.
    assert_eq!(titles(&page.body["items"]), ["The Long Road", "Middle Book"]);
    assert!(items.iter().all(|i| i["kind"] == "audiobook"), "{items:?}");
    assert_eq!(items[0]["authors"], json!(["Author A"]));
    assert_eq!(items[0]["year"], 2006);
    assert_eq!(items[1]["authors"], json!(["Author B"]));

    let ids = app.ids(lib, &token).await;
    let road = ids["The Long Road"];
    let detail = app.get(&format!("/api/items/{road}"), &token).await;
    assert_eq!(detail.status, StatusCode::OK);
    assert_eq!(
        detail.body["book"],
        json!({"authors": ["Author A"], "narrators": [], "series": "Series X", "series_index": 1.0})
    );
    let parts = app.get(&format!("/api/items/{road}/children"), &token).await;
    assert_eq!(titles(&parts.body["items"]), ["Prologue", "Chapter One"]);
    assert!(parts.body["items"].as_array().unwrap().iter().all(|p| p["kind"] == "part"));
    assert_eq!(parts.body["items"][0]["parent_title"], "The Long Road");

    // The single file is a book of one part, named as it is.
    let middle = ids["Middle Book"];
    let parts = app.get(&format!("/api/items/{middle}/children"), &token).await;
    assert_eq!(titles(&parts.body["items"]), ["Middle Book"]);
    let detail = app.get(&format!("/api/items/{middle}"), &token).await;
    assert_eq!(detail.body["book"]["series"], Value::Null);
    // Movies and shows have no book.
    assert_eq!(
        app.get(&format!("/api/items/{}", parts.body["items"][0]["id"]), &token).await.body["book"],
        Value::Null
    );
}

/// Play on a book takes it up in the part listened to last, where it was
/// left; a part finishes only near its end, not at 90 % as a film does,
/// and the next part comes after it. A book being listened to is in
/// Continue Listening, on Home and in its library, until it is finished.
#[tokio::test]
async fn play_takes_up_a_book_where_it_was_left() {
    let (app, token, lib) = books().await;
    let road = app.ids(lib, &token).await["The Long Road"];
    let parts = app.children(road, &token).await;
    let (prologue, one) = (parts["Prologue"], parts["Chapter One"]);

    assert_eq!(on_deck(&app, &token, road).await["id"], prologue, "a book not started starts at its beginning");
    assert!(hub(&app, &token, "/api/hubs/home", "continue_listening").await.is_empty());

    progress(&app, &token, prologue, 600_000).await;
    let deck = on_deck(&app, &token, road).await;
    assert_eq!((deck["id"].as_i64(), deck["user_state"]["position_ms"].as_i64()), (Some(prologue), Some(600_000)));
    assert_eq!(hub(&app, &token, "/api/hubs/home", "continue_listening").await, ["The Long Road"]);
    assert_eq!(
        hub(&app, &token, &format!("/api/libraries/{lib}/hubs"), "continue_listening").await,
        ["The Long Road"]
    );
    // Audiobooks aren't videos to continue watching.
    assert!(hub(&app, &token, "/api/hubs/home", "continue_watching").await.is_empty());

    // 95 % of an hour is three minutes before the end: still to hear.
    progress(&app, &token, prologue, 3_420_000).await;
    let state = &app.get(&format!("/api/items/{prologue}"), &token).await.body["user_state"];
    assert_eq!((state["watched"].as_bool(), state["position_ms"].as_i64()), (Some(false), Some(3_420_000)));

    // Its last seconds finish it, and the next part is next.
    progress(&app, &token, prologue, 3_590_000).await;
    assert_eq!(on_deck(&app, &token, road).await["id"], one);
    assert_eq!(hub(&app, &token, "/api/hubs/home", "continue_listening").await, ["The Long Road"]);
    let book = app.get(&format!("/api/items/{road}"), &token).await;
    assert_eq!((book.body["leaf_count"].as_i64(), book.body["viewed_leaf_count"].as_i64()), (Some(2), Some(1)));
    assert_eq!(book.body["user_state"]["watched"], false);

    // Finished, the book leaves Continue Listening.
    progress(&app, &token, one, 3_600_000).await;
    assert!(hub(&app, &token, "/api/hubs/home", "continue_listening").await.is_empty());
    assert_eq!(app.get(&format!("/api/items/{road}"), &token).await.body["user_state"]["watched"], true);

    // Taken out of Continue Listening, a book comes back when played again.
    progress(&app, &token, one, 60_000).await;
    assert_eq!(hub(&app, &token, "/api/hubs/home", "continue_listening").await, ["The Long Road"]);
    let hidden = app.delete(&format!("/api/continue-watching/{one}"), &token).await;
    assert_eq!(hidden.status, StatusCode::NO_CONTENT, "{:?}", hidden.body);
    assert!(hub(&app, &token, "/api/hubs/home", "continue_listening").await.is_empty());
    progress(&app, &token, one, 120_000).await;
    assert_eq!(hub(&app, &token, "/api/hubs/home", "continue_listening").await, ["The Long Road"]);
}

/// Plex's and Jellyfin's apps would read books as albums of tracks: their
/// APIs list no audiobook library.
#[tokio::test]
async fn compatibility_apis_leave_audiobooks_out() {
    let (app, token, lib) = books().await;
    app.media_file("movies/Some Movie (2001)/Some Movie (2001).mkv", 512);
    let movies = app.library(&token, "Movies", "movies", "movies").await;

    let sections = app.plex_json("/library/sections", &token).await;
    let names: Vec<&str> =
        sections["Directory"].as_array().unwrap().iter().map(|d| d["title"].as_str().unwrap()).collect();
    assert_eq!(names, ["Movies"]);
    let hubs = app.plex_json("/hubs", &token).await;
    assert_eq!(titles(&hubs["Hub"]), ["Recently Added in Movies"]);
    let book = app.ids(lib, &token).await["Middle Book"];
    let r = app.plex(Method::GET, &format!("/library/metadata/{book}"), &token, &[]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(
        app.plex(Method::GET, &format!("/library/sections/{lib}/all"), &token, &[]).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        movies,
        app.plex_json("/library/sections", &token).await["Directory"][0]["key"]
            .as_str()
            .unwrap()
            .parse::<i64>()
            .unwrap()
    );

    let jf = app.jellyfin_token(&token).await;
    let views = app.jf_json("/UserViews", &jf).await;
    let names: Vec<&str> = views["Items"].as_array().unwrap().iter().map(|v| v["Name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Movies"]);
}
