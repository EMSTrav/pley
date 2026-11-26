//! Artists' pictures, which MusicBrainz doesn't have, looked up by the
//! artist's MusicBrainz id: fanart.tv's (<https://fanart.tv>) with the key
//! an administrator gets there for free, then TheAudioDB's
//! (<https://www.theaudiodb.com>) with its shared free key, for what
//! fanart.tv lacks. Pictures in the artist's folder win over both.

use std::time::Duration;

use serde::Deserialize;

use super::{Api, ProviderError, Throttle, nullable};

pub const FANART: &str = "https://webservice.fanart.tv/v3";
/// TheAudioDB's free key ("123") is part of its address.
pub const AUDIODB: &str = "https://www.theaudiodb.com/api/v1/json/123";

/// An artist's picture and background, either missing.
#[derive(Debug, Default, PartialEq)]
pub struct Pictures {
    pub picture: Option<String>,
    pub background: Option<String>,
}

impl Pictures {
    fn complete(&self) -> bool {
        self.picture.is_some() && self.background.is_some()
    }

    /// What `other` has that this lacks.
    fn fill(&mut self, other: Pictures) {
        self.picture = self.picture.take().or(other.picture);
        self.background = self.background.take().or(other.background);
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FanartArtist {
    #[serde(deserialize_with = "nullable")]
    artistthumb: Vec<FanartImage>,
    #[serde(deserialize_with = "nullable")]
    artistbackground: Vec<FanartImage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FanartImage {
    url: String,
    /// A count, sent as a string.
    likes: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AudioDbAnswer {
    #[serde(deserialize_with = "nullable")]
    artists: Vec<AudioDbArtist>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct AudioDbArtist {
    str_artist_thumb: Option<String>,
    str_artist_fanart: Option<String>,
}

pub struct ArtistPictures {
    fanart: Api,
    fanart_base: String,
    audiodb: Api,
    audiodb_base: String,
}

impl ArtistPictures {
    pub fn new(http: reqwest::Client, fanart: &str, audiodb: &str) -> Self {
        Self {
            fanart: Api::new(http.clone(), "fanart.tv", Throttle::new(10, Duration::from_secs(1))),
            fanart_base: fanart.trim_end_matches('/').to_owned(),
            // The free key allows 30 requests a minute.
            audiodb: Api::new(http, "TheAudioDB", Throttle::new(30, Duration::from_secs(60))),
            audiodb_base: audiodb.trim_end_matches('/').to_owned(),
        }
    }

    /// The pictures of the artist with MusicBrainz id `mbid`: fanart.tv's
    /// when `fanart_key` isn't empty, TheAudioDB's for what is still
    /// missing. A service that fails or has nothing leaves that to the
    /// other: the artist simply gets fewer pictures.
    pub async fn find(&self, mbid: &str, fanart_key: &str) -> Pictures {
        let mut found = Pictures::default();
        if !fanart_key.is_empty() {
            match self.fanart_pictures(mbid, fanart_key).await {
                Ok(pictures) => found = pictures,
                Err(ProviderError::NotFound) => {}
                Err(err) => tracing::debug!("fanart.tv pictures of artist {mbid}: {err:#}"),
            }
        }
        if !found.complete() {
            match self.audiodb_pictures(mbid).await {
                Ok(pictures) => found.fill(pictures),
                Err(ProviderError::NotFound) => {}
                Err(err) => tracing::debug!("TheAudioDB pictures of artist {mbid}: {err:#}"),
            }
        }
        found
    }

    /// Whether fanart.tv takes `key`: the pictures of an artist it has.
    pub async fn check_fanart(&self, key: &str) -> Result<(), ProviderError> {
        // Daft Punk, whom fanart.tv has pictures of.
        self.fanart_pictures("056e4f3e-d505-4dad-8ec1-d04f521cbb56", key).await.map(|_| ())
    }

    async fn fanart_pictures(&self, mbid: &str, key: &str) -> Result<Pictures, ProviderError> {
        // The key goes in a header, not the address, which errors may show.
        let request = self.fanart.http.get(format!("{}/music/{mbid}", self.fanart_base)).header("api-key", key);
        let artist: FanartArtist = self.fanart.json(request).await?;
        Ok(Pictures { picture: most_liked(&artist.artistthumb), background: most_liked(&artist.artistbackground) })
    }

    async fn audiodb_pictures(&self, mbid: &str) -> Result<Pictures, ProviderError> {
        let request = self.audiodb.http.get(format!("{}/artist-mb.php", self.audiodb_base)).query(&[("i", mbid)]);
        let answer: AudioDbAnswer = self.audiodb.json(request).await?;
        let artist = answer.artists.into_iter().next().ok_or(ProviderError::NotFound)?;
        let picture = |url: Option<String>| url.map(|u| u.trim().to_owned()).filter(|u| !u.is_empty());
        Ok(Pictures { picture: picture(artist.str_artist_thumb), background: picture(artist.str_artist_fanart) })
    }
}

/// The picture fanart.tv's visitors liked most; the first of those liked as much.
fn most_liked(images: &[FanartImage]) -> Option<String> {
    let likes = |i: &FanartImage| i.likes.as_deref().and_then(|l| l.trim().parse::<u64>().ok()).unwrap_or(0);
    let mut best: Option<&FanartImage> = None;
    for image in images.iter().filter(|i| !i.url.trim().is_empty()) {
        if best.is_none_or(|b| likes(image) > likes(b)) {
            best = Some(image);
        }
    }
    best.map(|i| i.url.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_most_liked_picture_is_chosen() {
        let answer: FanartArtist = serde_json::from_str(
            r#"{"name": "Band", "artistthumb": [
                {"id": "1", "url": "https://assets.fanart.tv/a.jpg", "likes": "2"},
                {"id": "2", "url": "https://assets.fanart.tv/b.jpg", "likes": "7"},
                {"id": "3", "url": "https://assets.fanart.tv/c.jpg", "likes": "7"},
                {"id": "4", "url": " ", "likes": "40"}],
               "artistbackground": null}"#,
        )
        .unwrap();
        assert_eq!(most_liked(&answer.artistthumb).as_deref(), Some("https://assets.fanart.tv/b.jpg"));
        assert_eq!(most_liked(&answer.artistbackground), None);
    }

    #[test]
    fn one_service_fills_in_what_the_other_lacks() {
        let mut found = Pictures { picture: Some("a".into()), background: None };
        found.fill(Pictures { picture: Some("b".into()), background: Some("c".into()) });
        assert_eq!(found, Pictures { picture: Some("a".into()), background: Some("c".into()) });
        assert!(found.complete());
    }
}
