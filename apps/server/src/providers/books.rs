//! Audiobooks' metadata from Audible's catalog and, for books Audible
//! doesn't have, Open Library (<https://openlibrary.org/developers/api>).
//! Neither needs a key.
//!
//! Audible's catalog API is the one its own apps use: it isn't documented
//! or promised, but it has what is particular to audiobooks (narrators, a
//! book's place in its series, its length, the cover of the recording). It
//! is asked in the store of the server's country (`api.audible.co.uk` for
//! the United Kingdom), and books in the metadata language are preferred,
//! so that a translation doesn't win over the original. Open Library's data
//! is public domain (CC0); it describes printed books, so it fills in only
//! the summary, the year and a cover.
//!
//! As with music, the files' tags win: what is fetched fills in what the
//! book lacks. Files ripped from Audible carry the book's ASIN, which saves
//! the search.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;
use reqwest::header::ACCEPT;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{
    Api, ProviderError, Throttle, artwork, full_date, loose_id, loose_text, nullable, strip_html, title_score, year_of,
};
use crate::db::Db;
use crate::library::LibraryRow;
use crate::store::Store;
use crate::util::{normalize_key, now};

/// Audible's catalog; `{tld}` is the store's domain (`com`, `co.uk`, …).
pub const AUDIBLE: &str = "https://api.audible.{tld}";
pub const OPEN_LIBRARY: &str = "https://openlibrary.org";
pub const OPEN_LIBRARY_COVERS: &str = "https://covers.openlibrary.org";
/// What Audible is asked to answer with.
const RESPONSE_GROUPS: &str =
    "contributors,product_desc,product_extended_attrs,product_attrs,media,series,category_ladders";
/// Results looked through for a search.
const RESULTS: &str = "10";
/// Genres kept for a book.
const MAX_GENRES: usize = 5;
/// A library pass stops after this many failures in a row (neither source
/// answers); what's left is tried after the next scan.
const MAX_FAILURES: u32 = 5;
/// What the scanner calls a book's writer when nothing names one.
const UNKNOWN_AUTHOR: &str = "Unknown Author";

/// Where and in which language books are looked up: the server's country
/// and metadata language (see [`crate::ratings::region_in`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    /// ISO 3166-1 alpha-2, "GB".
    pub country: String,
    /// The metadata language, "en" or "en-US".
    pub language: String,
}

pub struct Books {
    audible: Api,
    audible_base: String,
    openlibrary: Api,
    openlibrary_base: String,
    covers: String,
    db: Db,
    store: Arc<Store>,
}

// ------------------------------------------------------------------ Audible's answers

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Search {
    #[serde(deserialize_with = "nullable")]
    products: Vec<Product>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Lookup {
    product: Option<Product>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Product {
    asin: String,
    title: Option<String>,
    subtitle: Option<String>,
    #[serde(deserialize_with = "nullable")]
    authors: Vec<Person>,
    #[serde(deserialize_with = "nullable")]
    narrators: Vec<Person>,
    #[serde(deserialize_with = "nullable")]
    series: Vec<SeriesRef>,
    language: Option<String>,
    release_date: Option<String>,
    #[serde(deserialize_with = "loose_id")]
    runtime_length_min: Option<i64>,
    publisher_summary: Option<String>,
    merchandising_summary: Option<String>,
    #[serde(deserialize_with = "nullable")]
    product_images: HashMap<String, String>,
    #[serde(deserialize_with = "nullable")]
    category_ladders: Vec<Ladders>,
    content_type: Option<String>,
    /// "unabridged", "abridged", "original_recording" (a dramatization),
    /// "highlights".
    format_type: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Person {
    name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SeriesRef {
    title: String,
    #[serde(deserialize_with = "loose_text")]
    sequence: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Ladders {
    root: Option<String>,
    #[serde(deserialize_with = "nullable")]
    ladder: Vec<Category>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Category {
    name: String,
}

// ------------------------------------------------------------------ Open Library's answers

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WorkSearch {
    #[serde(deserialize_with = "nullable")]
    docs: Vec<WorkDoc>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WorkDoc {
    key: String,
    title: String,
    subtitle: Option<String>,
    #[serde(deserialize_with = "nullable")]
    author_name: Vec<String>,
    #[serde(deserialize_with = "loose_id")]
    first_publish_year: Option<i64>,
    #[serde(deserialize_with = "loose_id")]
    cover_i: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Work {
    description: Option<Description>,
}

/// A work's description: text, or `{ "type": "/type/text", "value": text }`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Description {
    Text(String),
    Typed { value: String },
}

impl Description {
    fn text(&self) -> &str {
        match self {
            Description::Text(t) | Description::Typed { value: t } => t,
        }
    }
}

// ------------------------------------------------------------------ ours

#[derive(Debug, Default, sqlx::FromRow)]
struct BookRow {
    title: String,
    duration_ms: Option<i64>,
    authors: Option<String>,
    narrators: Option<String>,
    series: Option<String>,
    series_index: Option<f64>,
    poster: Option<String>,
    audible_asin: Option<String>,
    openlibrary_id: Option<String>,
}

/// A book as it is matched: what the files and folders say of it.
#[derive(Debug, Default, Clone)]
struct Book {
    title: String,
    /// Writers named (none when the scanner knows of none).
    authors: Vec<String>,
    narrators: Vec<String>,
    series: Option<String>,
    series_index: Option<f64>,
    duration_ms: Option<i64>,
}

impl Book {
    fn from_row(row: &BookRow) -> Self {
        let names =
            |json: Option<&str>| -> Vec<String> { json.and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default() };
        Self {
            title: row.title.clone(),
            authors: names(row.authors.as_deref()).into_iter().filter(|a| a != UNKNOWN_AUTHOR).collect(),
            narrators: names(row.narrators.as_deref()),
            series: row.series.clone(),
            series_index: row.series_index,
            duration_ms: row.duration_ms,
        }
    }
}

/// What a part's probe says about the book (`media_files.media_info`).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartInfo {
    tags: HashMap<String, String>,
    has_cover_art: bool,
}

/// What the book's files carry: an Audible ASIN, and cover art.
#[derive(Debug, Default)]
struct FileFacts {
    asin: Option<String>,
    cover: bool,
}

impl Books {
    pub fn new(
        http: reqwest::Client,
        audible: &str,
        openlibrary: &str,
        covers: &str,
        db: Db,
        store: Arc<Store>,
    ) -> Self {
        let base = |b: &str| b.trim_end_matches('/').to_owned();
        Self {
            // Neither says how fast it may be asked; both are asked gently.
            audible: Api::new(http.clone(), "Audible", Throttle::new(2, Duration::from_secs(1))),
            audible_base: base(audible),
            openlibrary: Api::new(http, "Open Library", Throttle::new(1, Duration::from_secs(1))),
            openlibrary_base: base(openlibrary),
            covers: base(covers),
            db,
            store,
        }
    }

    /// Look up the books of a library that have no metadata yet (all of
    /// them when `force`).
    pub async fn refresh_library(
        &self,
        lib: &LibraryRow,
        force: bool,
        wanted: &Wanted,
        progress: &(dyn Fn(u64, u64) + Sync),
    ) -> anyhow::Result<()> {
        let books: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM items WHERE library_id = ? AND kind = 'audiobook' AND metadata_locked = 0
               AND (? OR metadata_at IS NULL) ORDER BY added_at DESC",
        )
        .bind(lib.id)
        .bind(force)
        .fetch_all(&self.db.read)
        .await?;
        let total = books.len() as u64;
        if total == 0 {
            return Ok(());
        }
        tracing::info!("fetching metadata for {total} audiobooks in library {}", lib.name);
        let (mut done, mut failures) = (0u64, 0u32);
        for id in books {
            let result = self.refresh_book(id, wanted).await;
            done += 1;
            match result {
                Ok(()) => failures = 0,
                Err(ProviderError::NotFound) => {
                    failures = 0;
                    tracing::debug!("no Audible or Open Library match for item {id}");
                    sqlx::query("UPDATE items SET metadata_at = ? WHERE id = ?")
                        .bind(-now())
                        .bind(id)
                        .execute(&self.db.write)
                        .await?;
                }
                Err(err) => {
                    tracing::warn!("audiobook metadata for item {id} failed: {err:#}");
                    failures += 1;
                    if failures >= MAX_FAILURES {
                        bail!("neither Audible nor Open Library is answering; the rest waits for the next scan");
                    }
                }
            }
            // Hand edits win.
            crate::overrides::apply(&self.db, &[id]).await?;
            if done % 5 == 0 || done == total {
                progress(done, total);
            }
        }
        Ok(())
    }

    /// Refresh a book (or the book of a part).
    pub async fn refresh_item(&self, id: i64, wanted: &Wanted) -> Result<(), ProviderError> {
        let row: Option<(String, Option<i64>)> = sqlx::query_as("SELECT kind, parent_id FROM items WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.db.read)
            .await?;
        match row {
            Some((kind, _)) if kind == "audiobook" => self.refresh_book(id, wanted).await,
            Some((kind, Some(book))) if kind == "part" => self.refresh_book(book, wanted).await,
            _ => Ok(()),
        }
    }

    /// Audible first; Open Library for a book Audible doesn't have, or while
    /// Audible can't be reached.
    async fn refresh_book(&self, id: i64, wanted: &Wanted) -> Result<(), ProviderError> {
        let row: BookRow = sqlx::query_as(
            "SELECT title, duration_ms, authors, narrators, series, series_index, poster, audible_asin, openlibrary_id
             FROM items WHERE id = ? AND kind = 'audiobook'",
        )
        .bind(id)
        .fetch_optional(&self.db.read)
        .await?
        .ok_or(ProviderError::NotFound)?;
        let book = Book::from_row(&row);
        if normalize_key(&book.title).is_empty() {
            return Err(ProviderError::NotFound);
        }
        let files = self.file_facts(id).await?;

        let known = row.audible_asin.as_deref().filter(|a| is_asin(a)).map(str::to_owned).or(files.asin.clone());
        let audible = match known {
            Some(asin) => self.audible_product(&asin, wanted).await,
            None => self.audible_search(&book, wanted).await,
        };
        let audible_error = match audible {
            Ok(Some(product)) => return self.write_audible(id, &row, &files, product).await,
            Ok(None) => None,
            Err(err) => {
                tracing::debug!("Audible: {err:#}");
                Some(err)
            }
        };
        match self.openlibrary_work(&book, row.openlibrary_id.as_deref()).await {
            Ok(Some((doc, description))) => self.write_openlibrary(id, &row, &files, doc, description).await,
            // Audible not answering isn't "no such book": tried again later.
            Ok(None) => Err(audible_error.unwrap_or(ProviderError::NotFound)),
            Err(err) => Err(audible_error.unwrap_or(err)),
        }
    }

    // -------------------------------------------------------------- Audible

    fn audible_url(&self, country: &str, path: &str) -> String {
        format!("{}{path}", self.audible_base.replace("{tld}", audible_tld(country)))
    }

    async fn audible_product(&self, asin: &str, wanted: &Wanted) -> Result<Option<Product>, ProviderError> {
        let request = self
            .audible
            .http
            .get(self.audible_url(&wanted.country, &format!("/1.0/catalog/products/{asin}")))
            .query(&[("response_groups", RESPONSE_GROUPS), ("image_sizes", "500,1024")])
            .header(ACCEPT, "application/json");
        let found: Lookup = match self.audible.json(request).await {
            Err(ProviderError::NotFound) => return Ok(None),
            other => other?,
        };
        // Audible answers an ASIN it doesn't know with a product of no title.
        Ok(found.product.filter(|p| p.title.as_deref().is_some_and(|t| !t.trim().is_empty())))
    }

    async fn audible_search(&self, book: &Book, wanted: &Wanted) -> Result<Option<Product>, ProviderError> {
        let title = clean_title(&book.title);
        let mut query = vec![
            ("title", title.clone()),
            ("num_results", RESULTS.to_owned()),
            ("products_sort_by", "Relevance".to_owned()),
            ("response_groups", RESPONSE_GROUPS.to_owned()),
            ("image_sizes", "500,1024".to_owned()),
        ];
        if let Some(author) = book.authors.first() {
            query.push(("author", author.clone()));
        }
        let request = self
            .audible
            .http
            .get(self.audible_url(&wanted.country, "/1.0/catalog/products"))
            .query(&query)
            .header(ACCEPT, "application/json");
        let found: Search = self.audible.json(request).await?;
        Ok(best_product(found.products, book, &wanted.language))
    }

    async fn write_audible(
        &self,
        id: i64,
        row: &BookRow,
        files: &FileFacts,
        product: Product,
    ) -> Result<(), ProviderError> {
        let summary = product
            .publisher_summary
            .as_deref()
            .or(product.merchandising_summary.as_deref())
            .map(strip_html)
            .filter(|s| !s.is_empty());
        let genres = genres(&product.category_ladders);
        let genres = (!genres.is_empty()).then(|| serde_json::to_string(&genres).unwrap_or_default());
        let authors: Vec<String> =
            product.authors.iter().filter(|a| !is_credited_role(&a.name)).map(|a| a.name.trim().to_owned()).collect();
        let authors = (!authors.is_empty()).then(|| serde_json::to_string(&authors).unwrap_or_default());
        let narrators: Vec<&str> = product.narrators.iter().map(|n| n.name.trim()).filter(|n| !n.is_empty()).collect();
        let narrators = (!narrators.is_empty()).then(|| serde_json::to_string(&narrators).unwrap_or_default());
        let series = product.series.first().filter(|s| !s.title.trim().is_empty());
        let series_title = series.map(|s| s.title.trim().to_owned());
        let series_index = series.and_then(|s| sequence(s.sequence.as_deref()));
        // Covers in the files or next to them win.
        let image = product.product_images.get("1024").or_else(|| product.product_images.get("500"));
        let poster = if files.cover {
            None
        } else {
            artwork(&self.store, row.poster.as_deref(), image.map(String::as_str)).await
        };
        let now = now();
        // Every expression reads the row as it was: a series filled in brings
        // its place, and a place is filled in for the series the book has.
        sqlx::query(
            "UPDATE items SET audible_asin = ?, year = COALESCE(year, ?), release_date = COALESCE(release_date, ?),
                summary = COALESCE(summary, ?), genres = COALESCE(genres, ?), poster = COALESCE(?, poster),
                authors = CASE WHEN authors IS NULL OR authors IN ('[]', '[\"Unknown Author\"]')
                               THEN COALESCE(?, authors) ELSE authors END,
                narrators = CASE WHEN narrators IS NULL OR narrators = '[]' THEN COALESCE(?, narrators) ELSE narrators END,
                series = COALESCE(series, ?),
                series_index = CASE WHEN series IS NULL THEN ?
                                    WHEN series_index IS NULL AND lower(series) = lower(?) THEN ?
                                    ELSE series_index END,
                metadata_at = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(&product.asin)
        .bind(year_of(product.release_date.as_deref()))
        .bind(full_date(product.release_date.as_deref()))
        .bind(summary)
        .bind(genres)
        .bind(poster)
        .bind(authors)
        .bind(narrators)
        .bind(&series_title)
        .bind(series_index)
        .bind(&series_title)
        .bind(series_index)
        .bind(now)
        .bind(now)
        .bind(id)
        .execute(&self.db.write)
        .await?;
        Ok(())
    }

    // -------------------------------------------------------------- Open Library

    async fn ol_get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T, ProviderError> {
        let request = self
            .openlibrary
            .http
            .get(format!("{}{path}", self.openlibrary_base))
            .query(query)
            .header(ACCEPT, "application/json");
        self.openlibrary.json(request).await
    }

    /// The work for a book: the one it was matched to before, else the best
    /// search result; with its description.
    async fn openlibrary_work(
        &self,
        book: &Book,
        known: Option<&str>,
    ) -> Result<Option<(WorkDoc, Option<String>)>, ProviderError> {
        let doc = match known.filter(|k| is_work_id(k)) {
            Some(key) => WorkDoc { key: format!("/works/{key}"), ..WorkDoc::default() },
            None => {
                let title = clean_title(&book.title);
                let mut query = vec![
                    ("title", title.as_str()),
                    ("limit", "10"),
                    ("fields", "key,title,subtitle,author_name,first_publish_year,cover_i"),
                ];
                if let Some(author) = book.authors.first() {
                    query.push(("author", author.as_str()));
                }
                let found: WorkSearch = self.ol_get("/search.json", &query).await?;
                match best_work(found.docs, book) {
                    Some(doc) => doc,
                    None => return Ok(None),
                }
            }
        };
        let Some(key) = doc.key.strip_prefix("/works/").filter(|k| is_work_id(k)) else { return Ok(None) };
        let work: Work = match self.ol_get(&format!("/works/{key}.json"), &[]).await {
            Err(ProviderError::NotFound) => return Ok(None),
            other => other?,
        };
        let description = work.description.map(|d| clean_description(d.text())).filter(|d| !d.is_empty());
        Ok(Some((doc, description)))
    }

    async fn write_openlibrary(
        &self,
        id: i64,
        row: &BookRow,
        files: &FileFacts,
        doc: WorkDoc,
        description: Option<String>,
    ) -> Result<(), ProviderError> {
        let key = doc.key.trim_start_matches("/works/").to_owned();
        let cover = doc.cover_i.filter(|c| *c > 0).map(|c| format!("{}/b/id/{c}-L.jpg", self.covers));
        let poster =
            if files.cover { None } else { artwork(&self.store, row.poster.as_deref(), cover.as_deref()).await };
        let authors: Vec<&str> = doc.author_name.iter().map(|a| a.trim()).filter(|a| !a.is_empty()).collect();
        let authors = (!authors.is_empty()).then(|| serde_json::to_string(&authors).unwrap_or_default());
        let now = now();
        sqlx::query(
            "UPDATE items SET openlibrary_id = ?, year = COALESCE(year, ?), summary = COALESCE(summary, ?),
                poster = COALESCE(?, poster),
                authors = CASE WHEN authors IS NULL OR authors IN ('[]', '[\"Unknown Author\"]')
                               THEN COALESCE(?, authors) ELSE authors END,
                metadata_at = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(key)
        .bind(doc.first_publish_year)
        .bind(description)
        .bind(poster)
        .bind(authors)
        .bind(now)
        .bind(now)
        .bind(id)
        .execute(&self.db.write)
        .await?;
        Ok(())
    }

    // -------------------------------------------------------------- files

    async fn file_facts(&self, book: i64) -> sqlx::Result<FileFacts> {
        let infos: Vec<String> = sqlx::query_scalar(
            "SELECT f.media_info FROM media_files f JOIN items p ON p.id = f.item_id
             WHERE p.parent_id = ? AND f.media_info IS NOT NULL ORDER BY p.disc, p.idx LIMIT 5",
        )
        .bind(book)
        .fetch_all(&self.db.read)
        .await?;
        let mut out = FileFacts::default();
        for info in infos {
            let Ok(info) = serde_json::from_str::<PartInfo>(&info) else { continue };
            out.cover |= info.has_cover_art;
            out.asin = out.asin.or_else(|| {
                ["asin", "audible_asin", "audibleasin"]
                    .iter()
                    .find_map(|k| info.tags.get(*k))
                    .map(|v| v.trim().to_ascii_uppercase())
                    .filter(|v| is_asin(v))
            });
        }
        Ok(out)
    }

    /// Whether Audible (in the server's store) and Open Library answer a search.
    pub async fn check(&self, wanted: &Wanted) -> Result<(), ProviderError> {
        let request = self
            .audible
            .http
            .get(self.audible_url(&wanted.country, "/1.0/catalog/products"))
            .query(&[("title", "pley"), ("num_results", "1")])
            .header(ACCEPT, "application/json");
        self.audible.json::<serde::de::IgnoredAny>(request).await?;
        self.ol_get::<serde::de::IgnoredAny>("/search.json", &[("title", "pley"), ("limit", "1"), ("fields", "key")])
            .await
            .map(|_| ())
    }
}

// ------------------------------------------------------------------ matching

/// Audible's store for a country: Audible sells in a dozen, and the United
/// States' store serves the rest.
pub fn audible_tld(country: &str) -> &'static str {
    match country.to_ascii_uppercase().as_str() {
        "GB" | "UK" | "IE" => "co.uk",
        "CA" => "ca",
        "AU" | "NZ" => "com.au",
        "IN" => "in",
        "DE" | "AT" | "CH" | "LI" => "de",
        "FR" | "BE" | "LU" | "MC" => "fr",
        "IT" | "SM" => "it",
        "ES" => "es",
        "JP" => "co.jp",
        _ => "com",
    }
}

/// Audible's name for the metadata language ("en-GB" → "english").
fn audible_language(language: &str) -> Option<&'static str> {
    let code = language.split(['-', '_']).next().unwrap_or_default().to_ascii_lowercase();
    Some(match code.as_str() {
        "en" => "english",
        "fr" => "french",
        "es" => "spanish",
        "de" => "german",
        "it" => "italian",
        "ja" => "japanese",
        "pt" => "portuguese",
        "nl" => "dutch",
        "sv" => "swedish",
        "da" => "danish",
        "no" | "nb" => "norwegian",
        "fi" => "finnish",
        "pl" => "polish",
        "ru" => "russian",
        "zh" => "mandarin_chinese",
        "hi" => "hindi",
        _ => return None,
    })
}

/// A book's title as stores sell it: without the "(Unabridged)" that rips
/// carry, which no catalog has in its titles.
fn clean_title(title: &str) -> String {
    let mut t = title.trim().to_owned();
    loop {
        let lower = t.to_ascii_lowercase();
        let cut = [
            "(unabridged)",
            "[unabridged]",
            "unabridged",
            "(abridged)",
            "[abridged]",
            "(dramatized adaptation)",
            "(dramatised adaptation)",
        ]
        .iter()
        .find(|s| lower.ends_with(*s))
        .map(|s| s.len());
        match cut {
            Some(len) => t = t[..t.len() - len].trim_end_matches([' ', '-', ':', ',']).to_owned(),
            None => return t,
        }
    }
}

/// A credit with a role after a dash, as Audible lists translators among
/// the authors ("Juan Trejo - traductor").
fn is_credited_role(name: &str) -> bool {
    name.contains(" - ")
}

/// A name's words in order, so that "Tolkien, J. R. R." and "J.R.R. Tolkien"
/// are the same writer.
fn name_words(name: &str) -> Vec<String> {
    let name = name.split(" - ").next().unwrap_or(name);
    let mut words: Vec<String> = normalize_key(name).split(' ').filter(|w| !w.is_empty()).map(str::to_owned).collect();
    words.sort();
    words
}

fn same_person(a: &str, b: &str) -> bool {
    let (a, b) = (name_words(a), name_words(b));
    !a.is_empty() && a == b
}

fn any_same<'a>(ours: &[String], theirs: impl IntoIterator<Item = &'a str> + Clone) -> bool {
    ours.iter().any(|o| theirs.clone().into_iter().any(|t| same_person(o, t)))
}

/// A series' place ("6", "1.5", "Book 3"): its first number.
fn sequence(value: Option<&str>) -> Option<f64> {
    let value = value?;
    let start = value.find(|c: char| c.is_ascii_digit())?;
    let number: String = value[start..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    number.trim_end_matches('.').parse().ok().filter(|n: &f64| n.is_finite() && *n >= 0.0)
}

/// How well an Audible product fits the book; `None` when it is another
/// book: another title, or a writer the book doesn't have.
fn product_score(product: &Product, book: &Book, language: Option<&str>) -> Option<f64> {
    let title = product.title.as_deref()?.trim();
    if title.is_empty() || matches!(product.content_type.as_deref(), Some("Podcast" | "PodcastEpisode")) {
        return None;
    }
    let full = product.subtitle.as_deref().map(|s| format!("{title}: {s}"));
    let wanted = clean_title(&book.title);
    let mut score = title_score(&wanted, std::iter::once(title).chain(full.as_deref()));
    if score == 0.0 {
        return None;
    }
    if !book.authors.is_empty() {
        if !any_same(&book.authors, product.authors.iter().map(|a| a.name.as_str())) {
            return None;
        }
        score += 2.0;
    }
    // The metadata language's recording over a translation.
    match (language, product.language.as_deref()) {
        (Some(want), Some(got)) if got.eq_ignore_ascii_case(want) => score += 2.0,
        (Some(_), Some(_)) => score -= 2.0,
        _ => {}
    }
    // The book read whole, unless the files' length says otherwise.
    match product.format_type.as_deref() {
        Some("unabridged") => score += 0.5,
        Some("abridged" | "highlights") => score -= 1.0,
        _ => {}
    }
    // The same recording runs as long as the files.
    if let (Some(ours), Some(minutes)) = (book.duration_ms, product.runtime_length_min.filter(|m| *m > 0)) {
        let theirs = minutes as f64 * 60_000.0;
        let off = (ours as f64 - theirs).abs() / theirs;
        score += if off <= 0.03 {
            2.0
        } else if off <= 0.1 {
            1.0
        } else if off > 0.3 {
            -1.0
        } else {
            0.0
        };
    }
    if !book.narrators.is_empty() && any_same(&book.narrators, product.narrators.iter().map(|n| n.name.as_str())) {
        score += 1.0;
    }
    if let Some(series) = &book.series {
        let theirs = product.series.iter().find(|s| normalize_key(&s.title) == normalize_key(series));
        if let Some(theirs) = theirs {
            score += 1.0;
            if book.series_index.is_some() && book.series_index == sequence(theirs.sequence.as_deref()) {
                score += 1.0;
            }
        }
    }
    Some(score)
}

fn best_product(products: Vec<Product>, book: &Book, language: &str) -> Option<Product> {
    let language = audible_language(language);
    let mut best: Option<(f64, Product)> = None;
    for product in products {
        let Some(score) = product_score(&product, book, language) else { continue };
        if best.as_ref().is_none_or(|(b, _)| score > *b) {
            best = Some((score, product));
        }
    }
    best.map(|(_, p)| p)
}

/// The Open Library work for a book: the same title (or one holding the
/// other), by one of its writers when it names any.
fn best_work(docs: Vec<WorkDoc>, book: &Book) -> Option<WorkDoc> {
    let wanted = clean_title(&book.title);
    let mut best: Option<(f64, WorkDoc)> = None;
    for doc in docs {
        let full = doc.subtitle.as_deref().map(|s| format!("{}: {s}", doc.title));
        let mut score = title_score(&wanted, std::iter::once(doc.title.as_str()).chain(full.as_deref()));
        if score == 0.0 || !is_work_id(doc.key.trim_start_matches("/works/")) {
            continue;
        }
        if !book.authors.is_empty() {
            if !any_same(&book.authors, doc.author_name.iter().map(String::as_str)) {
                continue;
            }
            score += 2.0;
        }
        // A work with a cover is the edition people know.
        if doc.cover_i.is_some() {
            score += 0.5;
        }
        if best.as_ref().is_none_or(|(b, _)| score > *b) {
            best = Some((score, doc));
        }
    }
    best.map(|(_, d)| d)
}

/// Audible's genres: each category's second step ("Science Fiction" under
/// "Science Fiction & Fantasy"), or its first when it has one alone.
fn genres(ladders: &[Ladders]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in ladders.iter().filter(|l| l.root.as_deref().is_none_or(|r| r.eq_ignore_ascii_case("genres"))) {
        let Some(category) = l.ladder.get(1).or_else(|| l.ladder.first()) else { continue };
        let name = category.name.trim();
        if !name.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(name)) {
            out.push(name.to_owned());
        }
        if out.len() == MAX_GENRES {
            break;
        }
    }
    out
}

/// Open Library's description as plain text: without the links and the
/// "([source][1])" notes contributors add below it.
fn clean_description(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        // Markdown link definitions ("[1]: https://…") and separators end it.
        if t.starts_with("----------") || (t.starts_with('[') && t.contains("]: http")) {
            break;
        }
        lines.push(line);
    }
    strip_html(&lines.join("\n"))
}

/// An Amazon/Audible id: ten letters and digits ("B08G9PRS1K").
fn is_asin(value: &str) -> bool {
    value.len() == 10 && value.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// An Open Library work id: "OL21745884W".
fn is_work_id(value: &str) -> bool {
    value.len() > 3
        && value.starts_with("OL")
        && value.ends_with('W')
        && value[2..value.len() - 1].bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn product(json: serde_json::Value) -> Product {
        serde_json::from_value(json).unwrap()
    }

    fn book(title: &str, authors: &[&str]) -> Book {
        Book { title: title.into(), authors: authors.iter().map(|a| a.to_string()).collect(), ..Book::default() }
    }

    #[test]
    fn stores_follow_the_country() {
        assert_eq!(audible_tld("GB"), "co.uk");
        assert_eq!(audible_tld("nz"), "com.au");
        assert_eq!(audible_tld("AT"), "de");
        assert_eq!(audible_tld("US"), "com");
        assert_eq!(audible_tld("BR"), "com");
        assert_eq!(audible_tld(""), "com");
    }

    #[test]
    fn titles_lose_what_rips_add() {
        assert_eq!(clean_title("Dungeon Crawler Carl (Unabridged)"), "Dungeon Crawler Carl");
        assert_eq!(clean_title("Dune - Unabridged"), "Dune");
        assert_eq!(clean_title("Dune [Abridged] (Unabridged)"), "Dune");
        assert_eq!(clean_title("The Abridged Life"), "The Abridged Life");
    }

    #[test]
    fn writers_match_however_their_names_are_written() {
        assert!(same_person("Tolkien, J. R. R.", "J.R.R. Tolkien"));
        assert!(same_person("Andy Weir", "andy weir"));
        assert!(same_person("Matt Dinniman", "Matt Dinniman - foreword"));
        assert!(!same_person("Andy Weir", "Andy Warhol"));
        assert!(!same_person("", ""));
    }

    #[test]
    fn places_in_a_series() {
        assert_eq!(sequence(Some("6")), Some(6.0));
        assert_eq!(sequence(Some("1.5")), Some(1.5));
        assert_eq!(sequence(Some("Book 3")), Some(3.0));
        assert_eq!(sequence(Some("2.")), Some(2.0));
        assert_eq!(sequence(Some("Prequel")), None);
        assert_eq!(sequence(None), None);
    }

    /// The original over a translation, the recording as long as the files
    /// over another, and never another writer's book of the same name.
    #[test]
    fn the_right_recording_is_chosen() {
        let original = json_product("B0001", "The Lantern Sea", "english", 120, "Ada Quill");
        let translated = json_product("B0002", "The Lantern Sea", "spanish", 130, "Ada Quill");
        let abridged = json_product("B0003", "The Lantern Sea", "english", 60, "Ada Quill");
        let other = json_product("B0004", "The Lantern Sea", "english", 120, "Someone Else");
        let mut wanted = book("The Lantern Sea (Unabridged)", &["Ada Quill"]);
        wanted.duration_ms = Some(119 * 60_000);
        let found = best_product(vec![translated, abridged, other, original], &wanted, "en-US").unwrap();
        assert_eq!(found.asin, "B0001");

        // In Spanish, the Spanish recording.
        let original = json_product("B0001", "The Lantern Sea", "english", 120, "Ada Quill");
        let translated = json_product("B0002", "The Lantern Sea", "spanish", 130, "Ada Quill");
        assert_eq!(
            best_product(vec![original, translated], &book("The Lantern Sea", &["Ada Quill"]), "es").unwrap().asin,
            "B0002"
        );

        // Another writer's book, or another title: none.
        let other = json_product("B0004", "The Lantern Sea", "english", 120, "Someone Else");
        assert!(best_product(vec![other], &book("The Lantern Sea", &["Ada Quill"]), "en").is_none());
        let other = json_product("B0005", "Small Hours", "english", 60, "Ada Quill");
        assert!(best_product(vec![other], &book("The Lantern Sea", &["Ada Quill"]), "en").is_none());
        // A book by no one known takes the title alone.
        let any = json_product("B0001", "The Lantern Sea", "english", 120, "Ada Quill");
        assert_eq!(best_product(vec![any], &book("The Lantern Sea", &[]), "en").unwrap().asin, "B0001");
    }

    fn json_product(asin: &str, title: &str, language: &str, minutes: i64, author: &str) -> Product {
        product(serde_json::json!({
            "asin": asin, "title": title, "language": language, "runtime_length_min": minutes,
            "authors": [{"name": author}], "narrators": null, "series": null
        }))
    }

    /// Without the files' length, a reading of the whole book over a
    /// dramatization listed first; with it, the recording as long as they are.
    #[test]
    fn the_whole_book_unless_the_length_says_otherwise() {
        let products = || {
            vec![
                product(
                    serde_json::json!({"asin": "B0DRAMA", "title": "Small Hours", "authors": [{"name": "Imogen Vale"}],
                    "format_type": "original_recording", "runtime_length_min": 270}),
                ),
                product(
                    serde_json::json!({"asin": "B0WHOLE", "title": "Small Hours", "authors": [{"name": "Imogen Vale"}],
                    "format_type": "unabridged", "runtime_length_min": 700}),
                ),
            ]
        };
        let wanted = book("Small Hours", &["Imogen Vale"]);
        assert_eq!(best_product(products(), &wanted, "en").unwrap().asin, "B0WHOLE");
        let wanted = Book { duration_ms: Some(271 * 60_000), ..wanted };
        assert_eq!(best_product(products(), &wanted, "en").unwrap().asin, "B0DRAMA");
    }

    #[test]
    fn the_series_place_and_narrator_break_ties() {
        let one = product(serde_json::json!({
            "asin": "B0001", "title": "The Copper Bees", "authors": [{"name": "Marlow Finch"}],
            "series": [{"title": "Clockwork Orchard", "sequence": "1"}], "narrators": [{"name": "Someone"}]
        }));
        let two = product(serde_json::json!({
            "asin": "B0002", "title": "The Copper Bees", "authors": [{"name": "Marlow Finch"}],
            "series": [{"title": "Clockwork Orchard", "sequence": "2"}], "narrators": [{"name": "June Ashby"}]
        }));
        let mut wanted = book("The Copper Bees", &["Marlow Finch"]);
        wanted.series = Some("Clockwork Orchard".into());
        wanted.series_index = Some(2.0);
        wanted.narrators = vec!["June Ashby".into()];
        assert_eq!(best_product(vec![one, two], &wanted, "en").unwrap().asin, "B0002");
    }

    #[test]
    fn genres_are_each_categorys_second_step() {
        let ladders: Vec<Ladders> = serde_json::from_value(serde_json::json!([
            {"root": "Genres", "ladder": [{"name": "Science Fiction & Fantasy"}, {"name": "Science Fiction"}, {"name": "Adventure"}]},
            {"root": "Genres", "ladder": [{"name": "Science Fiction & Fantasy"}, {"name": "Science Fiction"}]},
            {"root": "Genres", "ladder": [{"name": "Literature & Fiction"}]},
            {"root": "Editors' Picks", "ladder": [{"name": "Best of 2021"}, {"name": "Fiction"}]}
        ]))
        .unwrap();
        assert_eq!(genres(&ladders), ["Science Fiction", "Literature & Fiction"]);
    }

    #[test]
    fn open_library_works_by_the_books_writer() {
        let docs: Vec<WorkDoc> = serde_json::from_value(serde_json::json!([
            {"key": "/works/OL1W", "title": "Small Hours", "author_name": ["Someone Else"], "cover_i": 9},
            {"key": "/works/OL2W", "title": "Small Hours", "author_name": ["Imogen Vale"]},
            {"key": "/works/OL3W", "title": "Small Hours", "author_name": ["Imogen Vale"], "cover_i": 7},
            {"key": "/books/OL4M", "title": "Small Hours", "author_name": ["Imogen Vale"], "cover_i": 7}
        ]))
        .unwrap();
        assert_eq!(best_work(docs, &book("Small Hours", &["Imogen Vale"])).unwrap().key, "/works/OL3W");
    }

    #[test]
    fn descriptions_and_ids() {
        assert_eq!(
            clean_description(
                "A keeper's tale.\r\n\r\n----------\r\nAlso contained in: [Collected][1]\r\n[1]: https://x"
            ),
            "A keeper's tale."
        );
        let typed: Work = serde_json::from_str(r#"{"description": {"type": "/type/text", "value": "Text."}}"#).unwrap();
        assert_eq!(typed.description.unwrap().text(), "Text.");
        let plain: Work = serde_json::from_str(r#"{"description": "Plain."}"#).unwrap();
        assert_eq!(plain.description.unwrap().text(), "Plain.");
        assert!(is_asin("B08G9PRS1K") && !is_asin("b08g9prs1k") && !is_asin("B08G9PRS1"));
        assert!(is_work_id("OL21745884W") && !is_work_id("OL21745884M") && !is_work_id("OLW"));
        assert!(is_credited_role("Juan Trejo - traductor") && !is_credited_role("Jean-Luc Picard"));
    }
}
