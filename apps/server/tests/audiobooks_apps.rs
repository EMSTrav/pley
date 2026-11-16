//! Who gets audiobooks: the web and desktop apps do, other apps (their
//! tokens) don't, neither the libraries nor their books on Home, in search
//! or in history, and still order the libraries they get.

mod common;

use axum::http::StatusCode;
use serde_json::{Value, json};

use common::{TestApp, encode, titles};

/// A library of films, one of books (a book in a folder of two parts and a
/// book in one `.m4b` file) and an empty one, made in that order.
struct Shelf {
    app: TestApp,
    admin: String,
    films: i64,
    books: i64,
    docs: i64,
}

async fn shelf() -> Shelf {
    let app = TestApp::new().await;
    let admin = app.setup_admin().await;
    app.media_file("films/First Film (2001)/First Film (2001).mkv", 2048);
    app.media_file("books/A Writer/Long Book/01 - Opening.mp3", 512);
    app.media_file("books/A Writer/Long Book/02 - Closing.mp3", 512);
    app.media_file("books/A Writer/Short Book.m4b", 512);
    let films = app.library(&admin, "Films", "movies", "films").await;
    let books = app.library(&admin, "Books", "audiobooks", "books").await;
    let docs = app.library(&admin, "Docs", "movies", "docs").await;
    Shelf { app, admin, films, books, docs }
}

/// `(book, its first part, its second part)` of the book in a folder.
async fn long_book(w: &Shelf) -> (i64, i64, i64) {
    let book = w.app.ids(w.books, &w.admin).await["Long Book"];
    let parts = w.app.children(book, &w.admin).await;
    (book, parts["Opening"], parts["Closing"])
}

/// A play of `item` reported stopped at `position_ms` of an hour.
async fn progress(app: &TestApp, token: &str, item: i64, position_ms: i64) {
    let body = json!({"position_ms": position_ms, "duration_ms": 3_600_000, "state": "stopped"});
    let r = app.post(&format!("/api/items/{item}/progress"), token, body).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{:?}", r.body);
}

fn names(libraries: &Value) -> Vec<String> {
    libraries.as_array().unwrap().iter().map(|l| l["name"].as_str().unwrap().to_owned()).collect()
}

async fn libraries(app: &TestApp, uri: &str, token: &str) -> Vec<String> {
    let r = app.get(uri, token).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    names(&r.body)
}

/// Home's hubs: `(key, titles)`.
async fn home(app: &TestApp, token: &str) -> Vec<(String, Vec<String>)> {
    let r = app.get("/api/hubs/home", token).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    r.body["hubs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["key"].as_str().unwrap().to_owned(), titles(&h["items"])))
        .collect()
}

/// What a search finds, by title, in its order.
async fn search(app: &TestApp, token: &str, q: &str) -> Vec<String> {
    let r = app.get(&format!("/api/search?q={}", encode(q)), token).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    titles(&r.body["items"])
}

async fn history(app: &TestApp, token: &str) -> Vec<String> {
    let r = app.get("/api/history", token).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    r.body["entries"].as_array().unwrap().iter().map(|e| e["item"]["title"].as_str().unwrap().to_owned()).collect()
}

/// Pley's own sign-in gets the library of books, a book being heard on
/// Home, books in search and parts in history. An app's token on the same
/// account, an administrator's with every scope it may have, gets none of
/// them: no library, nor what is in it on Home, in search or in history,
/// and asking for the library is `404`.
#[tokio::test]
async fn apps_get_no_audiobooks() {
    let w = shelf().await;
    let (_, opening, closing) = long_book(&w).await;
    let film = w.app.ids(w.films, &w.admin).await["First Film"];
    progress(&w.app, &w.admin, closing, 3_600_000).await;
    progress(&w.app, &w.admin, opening, 600_000).await;
    progress(&w.app, &w.admin, film, 3_500_000).await;

    assert_eq!(libraries(&w.app, "/api/libraries", &w.admin).await, ["Films", "Books", "Docs"]);
    let hubs = home(&w.app, &w.admin).await;
    assert!(hubs.contains(&("continue_listening".into(), vec!["Long Book".into()])), "{hubs:?}");
    assert!(hubs.iter().any(|(key, _)| *key == format!("recently_added:{}", w.books)), "{hubs:?}");
    assert_eq!(search(&w.app, &w.admin, "long book").await, ["Long Book"]);
    assert_eq!(history(&w.app, &w.admin).await, ["First Film", "Closing"]);

    let (token, _) = w.app.app_token(1, "library play server").await;
    assert_eq!(libraries(&w.app, "/api/libraries", &token).await, ["Films", "Docs"]);
    assert_eq!(libraries(&w.app, "/api/libraries?order=shared", &token).await, ["Films", "Docs"]);
    for uri in [
        format!("/api/libraries/{}", w.books),
        format!("/api/libraries/{}/items", w.books),
        format!("/api/libraries/{}/hubs", w.books),
        format!("/api/libraries/{}/filters", w.books),
        format!("/api/libraries/{}/collections", w.books),
    ] {
        assert_eq!(w.app.get(&uri, &token).await.status, StatusCode::NOT_FOUND, "{uri}");
    }
    let hubs = home(&w.app, &token).await;
    let keys: Vec<&str> = hubs.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, [format!("recently_added:{}", w.films)], "{hubs:?}");
    assert!(search(&w.app, &token, "book").await.is_empty());
    assert_eq!(search(&w.app, &token, "first film").await, ["First Film"]);
    assert_eq!(history(&w.app, &token).await, ["First Film"]);

    // So does another account's app, and its library alone answers.
    let (_, ann) = w.app.user(&w.admin, "ann", json!({"all_libraries": true})).await;
    let (anns, _) = w.app.app_token(ann, "library play").await;
    assert_eq!(libraries(&w.app, "/api/libraries", &anns).await, ["Films", "Docs"]);
    assert_eq!(w.app.get(&format!("/api/libraries/{}/items", w.films), &anns).await.status, StatusCode::OK);
    assert_eq!(w.app.get(&format!("/api/libraries/{}/items", w.books), &anns).await.status, StatusCode::NOT_FOUND);
}

/// An app orders the libraries it gets, its user's own order and, with
/// `server`, everyone's: the library of books it doesn't get keeps its
/// place in both, and naming it is refused as a library it doesn't have.
#[tokio::test]
async fn apps_order_the_libraries_they_get() {
    let w = shelf().await;
    let (token, _) = w.app.app_token(1, "library play server").await;

    // Everyone's: Films, Books, Docs as made.
    let order = w.app.put("/api/libraries/order", &token, json!({"ids": [w.docs, w.films]})).await;
    assert_eq!(order.status, StatusCode::OK, "{:?}", order.body);
    assert_eq!(names(&order.body), ["Docs", "Films"]);
    assert_eq!(libraries(&w.app, "/api/libraries?order=shared", &w.admin).await, ["Docs", "Books", "Films"]);
    let all = w.app.put("/api/libraries/order", &token, json!({"ids": [w.films, w.books, w.docs]})).await;
    assert_eq!(all.status, StatusCode::CONFLICT, "{:?}", all.body);
    // Pley's own sign-in still orders them all.
    let order = w.app.put("/api/libraries/order", &w.admin, json!({"ids": [w.films, w.docs, w.books]})).await;
    assert_eq!(names(&order.body), ["Films", "Docs", "Books"]);

    // The user's own, from the order they see now.
    let mine = w.app.put("/api/me/libraries/order", &w.admin, json!({"ids": [w.books, w.films, w.docs]})).await;
    assert_eq!(names(&mine.body), ["Books", "Films", "Docs"]);
    let mine = w.app.put("/api/me/libraries/order", &token, json!({"ids": [w.docs, w.films]})).await;
    assert_eq!(mine.status, StatusCode::OK, "{:?}", mine.body);
    assert_eq!(names(&mine.body), ["Docs", "Films"]);
    assert_eq!(libraries(&w.app, "/api/libraries", &w.admin).await, ["Books", "Docs", "Films"]);
    let all = w.app.put("/api/me/libraries/order", &token, json!({"ids": [w.books, w.docs, w.films]})).await;
    assert_eq!(all.status, StatusCode::CONFLICT, "{:?}", all.body);
    assert_eq!(libraries(&w.app, "/api/libraries", &w.admin).await, ["Books", "Docs", "Films"]);
}
