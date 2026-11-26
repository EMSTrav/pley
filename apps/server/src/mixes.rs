//! Mixes: a playlist made from a few artists someone chooses, as streaming
//! services make them. The tracks come from the libraries the user may open,
//! by those artists and the artists most like them (the genres they share).
//! With an AI service set up in the settings ([`Ai`]), the service picks and
//! orders tracks from that list and names the mix; without one, or when it
//! fails, Pley orders them itself ([`interleave`]).
//!
//! The AI service is sent the names of the tracks, albums and artists it may
//! choose from, their years and genres, the request's own words and the
//! language to name the mix in: never files, accounts or anything else. It
//! answers with ids from the list, so a mix holds only music the user has.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::db::Db;
use crate::error::AppError;
use crate::settings::Settings;

/// Artists a mix starts from, at most.
pub const MAX_SEEDS: usize = 5;
/// Tracks in a mix: the default and the most.
pub const DEFAULT_LENGTH: usize = 50;
pub const MAX_LENGTH: usize = 100;
/// The most of the request's own words ("for a long drive") sent on.
pub const MAX_PROMPT_CHARS: usize = 200;
/// Artists like the chosen ones a mix draws on, at most.
const SIMILAR_ARTISTS: usize = 15;
/// Tracks of each artist the mix may choose from: the chosen artists', the others'.
const SEED_TRACKS: i64 = 40;
const SIMILAR_TRACKS: i64 = 12;
/// How long the AI service may take to answer, and the most of its answer read.
const AI_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_AI_ANSWER: usize = 1 << 20;

/// A track a mix may hold.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Candidate {
    pub id: i64,
    pub title: String,
    pub album: Option<String>,
    pub artist_id: i64,
    pub artist: String,
    pub year: Option<i64>,
}

/// The AI service the settings name, when mixes may use one.
pub struct Ai {
    endpoint: String,
    model: String,
    key: String,
}

impl Ai {
    /// The service set up in `settings`, if it's on and has a model.
    pub fn from_settings(settings: &Settings) -> Option<Ai> {
        let endpoint = settings.ai_endpoint.trim().trim_end_matches('/');
        let model = settings.ai_model.trim();
        (settings.ai_mixes && !endpoint.is_empty() && !model.is_empty()).then(|| Ai {
            endpoint: endpoint.to_owned(),
            model: model.to_owned(),
            key: settings.ai_api_key.trim().to_owned(),
        })
    }

    /// An AI service at `endpoint` (an OpenAI-compatible API's base, such
    /// as `https://api.openai.com/v1`) asked for `model`, with `key` (empty
    /// for a local service without one).
    pub fn new(endpoint: &str, model: &str, key: &str) -> Ai {
        Ai {
            endpoint: endpoint.trim().trim_end_matches('/').to_owned(),
            model: model.trim().to_owned(),
            key: key.trim().to_owned(),
        }
    }

    /// The service's answer to `system` and `user`, which is asked to be JSON.
    async fn chat(&self, system: &str, user: &str) -> anyhow::Result<String> {
        let http = reqwest::Client::builder().timeout(AI_TIMEOUT).build()?;
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
            "response_format": {"type": "json_object"},
            "temperature": 0.8,
        });
        let mut request = http.post(format!("{}/chat/completions", self.endpoint)).json(&body);
        if !self.key.is_empty() {
            request = request.bearer_auth(&self.key);
        }
        // Errors leave the address out: it's the administrator's to keep.
        let resp = request.send().await.map_err(|e| anyhow!("the AI service can't be reached: {}", e.without_url()))?;
        let status = resp.status();
        let bytes = crate::util::read_body(resp, MAX_AI_ANSWER).await.context("the AI service's answer")?;
        if status.as_u16() == 401 || status.as_u16() == 403 {
            bail!("the AI service refused the key");
        }
        if !status.is_success() {
            let said = serde_json::from_slice::<serde_json::Value>(&bytes).ok().and_then(|v| {
                v["error"]["message"].as_str().or(v["error"].as_str()).map(|m| m.chars().take(200).collect::<String>())
            });
            bail!("the AI service answered HTTP {status}{}", said.map(|m| format!(": {m}")).unwrap_or_default());
        }
        #[derive(Deserialize)]
        struct Answer {
            choices: Vec<Choice>,
        }
        #[derive(Deserialize)]
        struct Choice {
            message: Message,
        }
        #[derive(Deserialize)]
        struct Message {
            content: Option<String>,
        }
        let answer: Answer = serde_json::from_slice(&bytes).context("the AI service's answer isn't a chat answer")?;
        answer
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or_else(|| anyhow!("the AI service said nothing"))
    }

    /// Whether the service answers: one short request.
    pub async fn check(&self) -> anyhow::Result<()> {
        let said = self.chat("Answer with JSON only.", r#"Answer {"ok": true}."#).await?;
        json_in(&said).map(|_| ()).ok_or_else(|| anyhow!("the AI service didn't answer with JSON"))
    }
}

/// What a mix is made from.
pub struct Request<'a> {
    pub artist_ids: &'a [i64],
    pub length: usize,
    /// Their own words: what the mix is for.
    pub prompt: &'a str,
    /// The language to name the mix in (as [`crate::i18n`] has them).
    pub language: &'static str,
}

/// A mix made: its name and its tracks, in order.
#[derive(Debug, PartialEq)]
pub struct Mix {
    pub title: String,
    pub track_ids: Vec<i64>,
    /// Whether the AI service chose them.
    pub by_ai: bool,
}

/// Make a mix for `user` from `request`, with `ai` when there is one. The
/// artists must be ones the user may see, in music libraries.
pub async fn make(db: &Db, user: &AuthUser, request: &Request<'_>, ai: Option<&Ai>) -> Result<Mix, AppError> {
    let libraries: Vec<i64> = crate::access::visible_libraries(db, user)
        .await?
        .into_iter()
        .filter(|l| l.kind() == crate::library::LibraryKind::Music)
        .map(|l| l.id)
        .collect();
    let libraries = serde_json::to_string(&libraries).unwrap_or_else(|_| "[]".into());
    let wanted = serde_json::to_string(request.artist_ids).unwrap_or_else(|_| "[]".into());
    let seeds: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, title FROM items
         WHERE kind = 'artist' AND id IN (SELECT value FROM json_each(?))
           AND library_id IN (SELECT value FROM json_each(?))",
    )
    .bind(&wanted)
    .bind(&libraries)
    .fetch_all(&db.read)
    .await?;
    if seeds.len() != request.artist_ids.iter().collect::<HashSet<_>>().len() {
        return Err(AppError::not_found("an artist chosen isn't in a music library you can open"));
    }
    // In the order they were chosen.
    let seeds: Vec<(i64, String)> =
        request.artist_ids.iter().filter_map(|id| seeds.iter().find(|(s, _)| s == id).cloned()).collect();
    let seed_ids: Vec<i64> = seeds.iter().map(|(id, _)| *id).collect();

    // Each artist's genres: theirs and their albums'.
    let genre_rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT CASE WHEN i.kind = 'album' THEN i.parent_id ELSE i.id END, g.genre
         FROM item_genres g JOIN items i ON i.id = g.item_id
         WHERE g.library_id IN (SELECT value FROM json_each(?)) AND g.kind IN ('artist', 'album')",
    )
    .bind(&libraries)
    .fetch_all(&db.read)
    .await?;
    let mut genres: HashMap<i64, HashSet<String>> = HashMap::new();
    for (artist, genre) in genre_rows {
        genres.entry(artist).or_default().insert(genre.to_lowercase());
    }
    let similar = similar_artists(&seed_ids, &genres, SIMILAR_ARTISTS);

    // The tracks to choose from, each artist's in a random order with those
    // played more often a little ahead.
    let pick = |artists: Vec<i64>, per_artist: i64| {
        let artists = serde_json::to_string(&artists).unwrap_or_else(|_| "[]".into());
        sqlx::query_as::<_, Candidate>(sqlx::AssertSqlSafe(format!(
            "{} SELECT id, title, album, artist_id, artist, year FROM (
                 SELECT t.id, t.title, al.title AS album, ar.id AS artist_id, ar.title AS artist,
                        COALESCE(al.year, t.year) AS year,
                        ROW_NUMBER() OVER (PARTITION BY ar.id
                            ORDER BY ABS(RANDOM()) % 1000 + MIN(COALESCE(us.play_count, 0), 5) * 150 DESC) AS n
                 FROM items t JOIN items ar ON ar.id = t.grandparent_id LEFT JOIN items al ON al.id = t.parent_id
                      LEFT JOIN user_item_state us ON us.item_id = t.id AND us.user_id = (SELECT uid FROM me)
                 WHERE t.kind = 'track' AND ar.id IN (SELECT value FROM json_each(?))
                   AND t.library_id IN (SELECT value FROM json_each(?)))
             WHERE n <= ? ORDER BY artist_id, n",
            crate::catalog::ME_CTE
        )))
        .bind(user.id)
        .bind(artists)
        .bind(libraries.clone())
        .bind(per_artist)
        .fetch_all(&db.read)
    };
    let mut candidates = pick(seed_ids.clone(), SEED_TRACKS).await?;
    candidates.extend(pick(similar.iter().map(|(id, _)| *id).collect(), SIMILAR_TRACKS).await?);
    if candidates.is_empty() {
        return Err(AppError::bad_request("those artists have no tracks to make a mix of"));
    }
    let length = request.length.clamp(1, MAX_LENGTH);
    let names: Vec<&str> = seeds.iter().map(|(_, name)| name.as_str()).collect();
    let fallback_title = || crate::i18n::t_with(request.language, "mixes.title", &[("artists", &names.join(", "))]);

    if let Some(ai) = ai {
        let (system, prompt) = ai_prompt(&names, &candidates, length, request.prompt, request.language);
        match ai.chat(&system, &prompt).await {
            Ok(said) => match read_ai_mix(&said, &candidates, length) {
                Some((title, track_ids)) if track_ids.len() * 2 >= length.min(candidates.len()) => {
                    return Ok(Mix { title: title.unwrap_or_else(fallback_title), track_ids, by_ai: true });
                }
                _ => tracing::warn!("the AI service's mix wasn't usable; Pley makes it instead"),
            },
            Err(err) => tracing::warn!("mix from the AI service failed, Pley makes it instead: {err:#}"),
        }
    }
    Ok(Mix { title: fallback_title(), track_ids: interleave(&candidates, &seed_ids, length), by_ai: false })
}

/// The artists most like `seeds`, best first, with how alike (0 to 1): the
/// share of genres they have in common with the seed they're most like.
/// Artists with no genre in common aren't alike.
pub fn similar_artists(seeds: &[i64], genres: &HashMap<i64, HashSet<String>>, most: usize) -> Vec<(i64, f64)> {
    let empty = HashSet::new();
    let mut alike: Vec<(i64, f64)> = genres
        .iter()
        .filter(|(id, _)| !seeds.contains(id))
        .filter_map(|(&id, theirs)| {
            let best = seeds
                .iter()
                .map(|seed| {
                    let ours = genres.get(seed).unwrap_or(&empty);
                    let shared = ours.intersection(theirs).count();
                    let all = ours.union(theirs).count();
                    if all == 0 { 0.0 } else { shared as f64 / all as f64 }
                })
                .fold(0.0, f64::max);
            (best > 0.0).then_some((id, best))
        })
        .collect();
    alike.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    alike.truncate(most);
    alike
}

/// A mix of `length` tracks from `candidates` (each artist's in the order
/// given) without an AI service: about two in three by the chosen artists,
/// taken in turn, the rest by the others, and one artist twice in a row
/// only when nothing else is left.
pub fn interleave(candidates: &[Candidate], seeds: &[i64], length: usize) -> Vec<i64> {
    let mut by_artist: Vec<(i64, std::collections::VecDeque<i64>)> = Vec::new();
    for c in candidates {
        match by_artist.iter_mut().find(|(a, _)| *a == c.artist_id) {
            Some((_, tracks)) => tracks.push_back(c.id),
            None => by_artist.push((c.artist_id, [c.id].into())),
        }
    }
    let (ours, others): (Vec<_>, Vec<_>) = by_artist.into_iter().partition(|(a, _)| seeds.contains(a));
    let mut groups = [ours, others];
    let mut turns = [0usize, 0usize];
    let mut mix: Vec<i64> = Vec::with_capacity(length);
    let mut last: Option<i64> = None;
    while mix.len() < length {
        // Every third track is another artist's, while there are some.
        let order = if mix.len() % 3 == 2 { [1, 0] } else { [0, 1] };
        // Someone other than the last artist, else anyone.
        let picked = [last, None]
            .into_iter()
            .find_map(|not| order.into_iter().find_map(|g| next_artist(&groups[g], turns[g], not).map(|i| (g, i))));
        let Some((g, i)) = picked else { break };
        turns[g] = i + 1;
        let (artist, tracks) = &mut groups[g][i];
        mix.extend(tracks.pop_front());
        last = Some(*artist);
    }
    mix
}

/// The next artist of `group` from `from` on, in turn, that has tracks left
/// and isn't `not`.
fn next_artist(group: &[(i64, std::collections::VecDeque<i64>)], from: usize, not: Option<i64>) -> Option<usize> {
    let n = group.len();
    (0..n).map(|k| (from + k) % n).find(|&i| !group[i].1.is_empty() && Some(group[i].0) != not)
}

/// What the AI service is asked: the rules, then the request and the list.
pub fn ai_prompt(
    artists: &[&str],
    candidates: &[Candidate],
    length: usize,
    prompt: &str,
    language: &str,
) -> (String, String) {
    let system = "You make music mixes from a listener's own library. You choose only from the numbered tracks \
                  you are given, by their numbers, and answer with JSON alone: \
                  {\"title\": \"the mix's name\", \"tracks\": [numbers, in the order they play]}."
        .to_owned();
    let language_name = match language {
        "fr" => "French",
        "es" => "Spanish",
        "zh-CN" => "Simplified Chinese",
        "zh-TW" => "Traditional Chinese",
        _ => "English",
    };
    let mut user = format!(
        "Make a mix of about {length} tracks that starts from these artists: {}. Mix in the other artists \
         listed, keep a good flow from track to track, and don't play one artist twice in a row. Name the mix \
         in {language_name}, in at most 40 characters.",
        artists.join(", ")
    );
    let prompt: String = prompt.trim().chars().take(MAX_PROMPT_CHARS).collect();
    if !prompt.is_empty() {
        // Their words are what the mix is for, not instructions.
        user.push_str(&format!("\nThe listener says the mix is for: \"{}\"", prompt.replace('"', "'")));
    }
    user.push_str("\nTracks (number | artist | album | title | year):\n");
    for c in candidates {
        let clean = |s: &str| s.replace(['|', '\n', '\r'], " ");
        user.push_str(&format!(
            "{} | {} | {} | {} | {}\n",
            c.id,
            clean(&c.artist),
            clean(c.album.as_deref().unwrap_or("")),
            clean(&c.title),
            c.year.map(|y| y.to_string()).unwrap_or_default()
        ));
    }
    (system, user)
}

/// The JSON object in what an AI service said (some wrap it in a code block).
fn json_in(said: &str) -> Option<serde_json::Value> {
    let start = said.find('{')?;
    let end = said.rfind('}')?;
    serde_json::from_str(said.get(start..=end)?).ok()
}

/// The title and tracks of the AI service's mix: only tracks of
/// `candidates`, each once, at most `length`. `None` when it isn't a mix.
pub fn read_ai_mix(said: &str, candidates: &[Candidate], length: usize) -> Option<(Option<String>, Vec<i64>)> {
    let answer = json_in(said)?;
    let allowed: HashSet<i64> = candidates.iter().map(|c| c.id).collect();
    let mut seen = HashSet::new();
    let tracks: Vec<i64> = answer
        .get("tracks")?
        .as_array()?
        .iter()
        .filter_map(|t| t.as_i64().or_else(|| t.as_str()?.trim().parse().ok()))
        .filter(|id| allowed.contains(id) && seen.insert(*id))
        .take(length)
        .collect();
    let title = answer
        .get("title")
        .and_then(|t| t.as_str())
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(100).collect::<String>())
        .filter(|t| !t.is_empty());
    (!tracks.is_empty()).then_some((title, tracks))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: i64, artist_id: i64) -> Candidate {
        Candidate {
            id,
            title: format!("Track {id}"),
            album: Some("Album".into()),
            artist_id,
            artist: format!("Artist {artist_id}"),
            year: Some(2001),
        }
    }

    fn genres(list: &[(i64, &[&str])]) -> HashMap<i64, HashSet<String>> {
        list.iter().map(|(id, g)| (*id, g.iter().map(|s| s.to_string()).collect())).collect()
    }

    #[test]
    fn artists_alike_share_genres() {
        let g = genres(&[
            (1, &["house", "electronic"]),
            (2, &["rock"]),
            (10, &["house", "electronic", "disco"]),
            (11, &["electronic", "ambient"]),
            (12, &["jazz"]),
            (13, &["rock", "punk"]),
        ]);
        let alike = similar_artists(&[1, 2], &g, 10);
        let ids: Vec<i64> = alike.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, [10, 13, 11], "jazz has nothing in common; the chosen artists aren't listed");
        assert!((alike[0].1 - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(similar_artists(&[1, 2], &g, 1).len(), 1);
        assert!(similar_artists(&[99], &g, 10).is_empty(), "an artist without genres has no one alike");
    }

    #[test]
    fn a_mix_without_ai_takes_turns_and_mixes_others_in() {
        let mut candidates: Vec<Candidate> = (0..10).map(|n| track(100 + n, 1)).collect();
        candidates.extend((0..10).map(|n| track(200 + n, 2)));
        candidates.extend((0..10).map(|n| track(300 + n, 3)));
        let mix = interleave(&candidates, &[1, 2], 9);
        let artists: Vec<i64> = mix.iter().map(|id| id / 100).collect();
        assert_eq!(artists, [1, 2, 3, 1, 2, 3, 1, 2, 3]);
        assert_eq!(&mix[..3], &[100, 200, 300], "each artist's tracks in the order given");
        assert!(artists.windows(2).all(|w| w[0] != w[1]), "never one artist twice in a row");
    }

    #[test]
    fn a_mix_plays_one_artist_twice_only_when_no_one_else_is_left() {
        let mut candidates: Vec<Candidate> = (0..5).map(|n| track(100 + n, 1)).collect();
        candidates.push(track(200, 2));
        let mix = interleave(&candidates, &[1], 50);
        assert_eq!(mix.len(), 6, "a mix holds what there is");
        assert_eq!(mix, [100, 200, 101, 102, 103, 104], "the other artist as soon as it can be");
    }

    #[test]
    fn the_ais_mix_holds_only_tracks_it_was_given() {
        let candidates = vec![track(1, 1), track(2, 1), track(3, 2)];
        let said =
            "Here you go:\n```json\n{\"title\": \"  Late   Night\\n Drive \", \"tracks\": [3, \"1\", 99, 3, 2]}\n```";
        assert_eq!(read_ai_mix(said, &candidates, 10), Some((Some("Late Night Drive".into()), vec![3, 1, 2])));
        assert_eq!(read_ai_mix(said, &candidates, 2), Some((Some("Late Night Drive".into()), vec![3, 1])));
        assert_eq!(read_ai_mix(r#"{"title": "x", "tracks": [42]}"#, &candidates, 10), None);
        assert_eq!(read_ai_mix("no mix today", &candidates, 10), None);
        assert_eq!(read_ai_mix(r#"{"tracks": [2]}"#, &candidates, 10), Some((None, vec![2])));
    }

    #[test]
    fn the_prompt_lists_the_tracks_and_quotes_the_request() {
        let mut odd = track(7, 1);
        odd.title = "A | B\nC".into();
        let (system, user) = ai_prompt(&["Artist 1"], &[odd], 25, " for \"running\" ", "fr");
        assert!(system.contains("JSON"));
        assert!(user.contains("about 25 tracks") && user.contains("Artist 1") && user.contains("in French"));
        assert!(user.contains("7 | Artist 1 | Album | A   B C | 2001"), "{user}");
        assert!(user.contains("is for: \"for 'running'\""), "{user}");
        let (_, plain) = ai_prompt(&["Artist 1"], &[track(1, 1)], 25, "  ", "en");
        assert!(!plain.contains("is for"));
    }
}
