//! Mixes: playlists made from a few artists (see `crate::mixes`).

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::error::{ApiResult, AppError};
use crate::mixes::{self, Ai, DEFAULT_LENGTH, MAX_LENGTH, MAX_PROMPT_CHARS, MAX_SEEDS};
use crate::playlists::{self, Kind};
use crate::state::AppState;

#[derive(Serialize)]
pub struct MixInfo {
    /// Whether an AI service chooses mixes' tracks (and reads `prompt`).
    ai: bool,
    max_artists: usize,
    max_length: usize,
}

/// `GET /api/mixes`: what making a mix offers.
pub async fn info(State(state): State<AppState>, _user: AuthUser) -> Json<MixInfo> {
    let ai = Ai::from_settings(&state.settings.get()).is_some();
    Json(MixInfo { ai, max_artists: MAX_SEEDS, max_length: MAX_LENGTH })
}

#[derive(Deserialize)]
pub struct NewMix {
    artist_ids: Vec<i64>,
    #[serde(default)]
    length: Option<usize>,
    /// What the mix is for, in their own words (read by the AI service).
    #[serde(default)]
    prompt: Option<String>,
}

#[derive(Serialize)]
pub struct MadeMix {
    playlist_id: i64,
    title: String,
    item_count: usize,
    by_ai: bool,
}

/// `POST /api/mixes`: make a mix from the artists and keep it as one of the
/// user's playlists.
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    headers: HeaderMap,
    Json(body): Json<NewMix>,
) -> ApiResult<(StatusCode, Json<MadeMix>)> {
    if body.artist_ids.is_empty() || body.artist_ids.len() > MAX_SEEDS {
        return Err(AppError::bad_request(format!("a mix starts from 1 to {MAX_SEEDS} artists")));
    }
    let length = body.length.unwrap_or(DEFAULT_LENGTH);
    if !(1..=MAX_LENGTH).contains(&length) {
        return Err(AppError::bad_request(format!("a mix holds 1 to {MAX_LENGTH} tracks")));
    }
    let prompt = body.prompt.as_deref().unwrap_or("").trim();
    if prompt.chars().count() > MAX_PROMPT_CHARS {
        return Err(AppError::bad_request(format!("say what the mix is for in at most {MAX_PROMPT_CHARS} characters")));
    }
    let settings = state.settings.get();
    let language = crate::i18n::for_user(&state.db, user.id, &settings.language, &headers).await?;
    let ai = Ai::from_settings(&settings);
    let request = mixes::Request { artist_ids: &body.artist_ids, length, prompt, language };
    let mix = mixes::make(&state.db, &user, &request, ai.as_ref()).await?;
    let id = playlists::create(&state.db, &user, &mix.title, Kind::Audio).await?;
    if let Err(err) = playlists::add(&state.db, &user, id, &mix.track_ids).await {
        playlists::delete(&state.db, &user, id).await?;
        return Err(err);
    }
    let made = MadeMix { playlist_id: id, title: mix.title, item_count: mix.track_ids.len(), by_ai: mix.by_ai };
    Ok((StatusCode::CREATED, Json(made)))
}
