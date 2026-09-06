use crate::{
    CachedEndpoint, ClientError, Endpoint, RestClient,
    remux::{MediaSegments, Segment},
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const BASE_URL: &str = "https://api.introdb.app";
const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IntroDbResponse {
    intro: Option<IntroDbInterval>,
    recap: Option<IntroDbInterval>,
    outro: Option<IntroDbInterval>,
}

impl IntroDbResponse {
    fn into_segments(self) -> MediaSegments {
        MediaSegments {
            intro: self
                .intro
                .and_then(IntroDbInterval::into_segment),
            recap: self
                .recap
                .and_then(IntroDbInterval::into_segment),
            outro: self
                .outro
                .and_then(IntroDbInterval::into_segment),
            ..MediaSegments::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IntroDbInterval {
    start_sec: Option<f64>,
    end_sec: Option<f64>,
}

impl IntroDbInterval {
    fn into_segment(self) -> Option<Segment> {
        Some(Segment {
            start_ticks: secs_to_ticks(self.start_sec?),
            end_ticks: secs_to_ticks(self.end_sec?),
        })
    }
}

fn secs_to_ticks(secs: f64) -> i64 {
    (secs * 10_000_000.0) as i64
}

#[derive(Clone, Serialize)]
struct EpisodeEndpoint {
    imdb_id: String,
    season: i64,
    episode: i64,
}

impl Endpoint for EpisodeEndpoint {
    type Output = IntroDbResponse;

    fn path(&self) -> String {
        format!(
            "/segments?imdb_id={}&season={}&episode={}",
            self.imdb_id, self.season, self.episode
        )
    }
}

pub async fn fetch_episode_segments(
    imdb_id: &str,
    season: i64,
    episode: i64,
) -> Result<MediaSegments> {
    let client = RestClient::new(BASE_URL)
        .map_err(|e| anyhow!("introdb client error: {e}"))?
        .with_retry(crate::ExponentialBackoff::builder().build_with_max_retries(3));

    let ep = EpisodeEndpoint {
        imdb_id: imdb_id.to_string(),
        season,
        episode,
    };
    let resp = match client
        .execute(ep.with_cache(CACHE_TTL))
        .await
    {
        Ok(r) => r,
        Err(ClientError::Http { status: 404, .. }) => {
            return Ok(MediaSegments::default());
        }
        Err(e) => return Err(anyhow!("introdb request failed: {e}")),
    };

    Ok(resp.into_segments())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn nested_segments_preserve_independent_intervals_and_fractional_ticks() {
        let server = httpmock::MockServer::start();
        let client = RestClient::new(&server.base_url()).unwrap();
        let intro = Segment {
            start_ticks: 122_500_000,
            end_ticks: 467_500_000,
        };
        let recap = Segment {
            start_ticks: 0,
            end_ticks: 115_000_000,
        };
        let outro = Segment {
            start_ticks: 34_311_250_000,
            end_ticks: 35_005_000_000,
        };
        let cases = [
            (
                json!({
                    "intro": {"start_sec": 12.25, "end_sec": 46.75},
                    "recap": {"start_sec": 0, "end_sec": 11.5},
                    "outro": {"start_sec": 3431.125, "end_sec": 3500.5}
                }),
                MediaSegments {
                    intro: Some(intro),
                    recap: Some(recap.clone()),
                    outro: Some(outro.clone()),
                    ..MediaSegments::default()
                },
            ),
            (
                json!({
                    "intro": null,
                    "outro": {"start_sec": 3431.125, "end_sec": 3500.5}
                }),
                MediaSegments {
                    outro: Some(outro.clone()),
                    ..MediaSegments::default()
                },
            ),
            (
                json!({
                    "intro": {"start_sec": 12.25},
                    "recap": {"start_sec": 0, "end_sec": 11.5},
                    "outro": {"start_sec": null, "end_sec": 3500.5}
                }),
                MediaSegments {
                    recap: Some(recap),
                    ..MediaSegments::default()
                },
            ),
        ];

        for (index, (body, expected)) in cases
            .into_iter()
            .enumerate()
        {
            let episode = (index + 1) as i64;
            let mock = server.mock(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/segments")
                    .query_param("imdb_id", "tt0903747")
                    .query_param("season", "1")
                    .query_param("episode", episode.to_string());
                then.status(200)
                    .json_body(body);
            });
            let response = client
                .execute(EpisodeEndpoint {
                    imdb_id: "tt0903747".to_string(),
                    season: 1,
                    episode,
                })
                .await
                .unwrap();

            assert_eq!(response.into_segments(), expected);
            mock.assert();
        }
    }
}
