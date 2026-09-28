//! Strict playback languages, opted into per user with the policy flag
//! `remux.require_english_spanish`.
//!
//! A strict user plays only sources whose local ffprobe shows English or
//! Spanish audio, or the title's original-language audio plus a full embedded
//! English/Spanish text subtitle. Filename tags, filename guesses and RemuxDB
//! crowd data are not evidence. Probe results persist in `media.probe_data`
//! (shared with ordinary playback probing), so each file is probed once.

use crate::{
    AppContext, api, db,
    playback::probe::{placeholder_threshold, probe_media_bounded},
};
use anyhow::anyhow;
use futures_util::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use remux_sdks::remux::{MediaStreamType, UserPolicy, lang_to_two_letter};
use std::{
    collections::{HashMap, HashSet},
    sync::{LazyLock, Mutex, PoisonError},
    time::Duration,
};
use tokio::{sync::Semaphore, time::Instant};
use tracing::debug;
use uuid::Uuid;

/// How long a playback request waits for its best-ranked source's verdict.
const PLAY_WAIT: Duration = Duration::from_secs(20);
/// A failed probe (dead, uncached, placeholder) is retried after this long.
const FAILURE_TTL: Duration = Duration::from_secs(10 * 60);

/// One ffprobe budget for all requests: clients fire PlaybackInfo in bursts.
static PROBES: Semaphore = Semaphore::const_new(3);
/// Most probes one request keeps running at a time.
const PROBE_WINDOW: usize = 3;
/// Concurrent requests for one title share each source's probe.
static IN_FLIGHT: LazyLock<Mutex<HashMap<Uuid, Probe>>> =
    LazyLock::new(Default::default);

type Probe = Shared<BoxFuture<'static, Option<api::MediaSourceInfo>>>;

pub(crate) fn enabled(policy: Option<&UserPolicy>) -> bool {
    policy
        .and_then(|p| {
            p.remux
                .as_ref()
        })
        .is_some_and(|p| p.require_english_spanish)
}

async fn enabled_for(ctx: &AppContext, user_id: Option<Uuid>) -> anyhow::Result<bool> {
    let Some(id) = user_id else { return Ok(false) };
    let user = db::User::get_by_id(&ctx.db, &id).await?;
    Ok(enabled(
        user.as_ref()
            .and_then(|u| {
                u.policy
                    .as_deref()
            }),
    ))
}

/// The sources `user_id` may play for `item`: all of them unless the user is
/// strict, in which case see `retain_playable`.
pub(crate) async fn playable_for(
    ctx: &AppContext,
    user_id: Option<Uuid>,
    item: &db::Media,
    sources: Vec<db::Media>,
) -> anyhow::Result<Vec<db::Media>> {
    if !matches!(item.kind, db::MediaKind::Movie | db::MediaKind::Episode)
        || !enabled_for(ctx, user_id).await?
    {
        return Ok(sources);
    }
    let original = original_language(ctx, item).await;
    Ok(retain_playable(ctx, sources, original.as_deref()).await)
}

/// Original language of a movie, or of an episode's series.
pub(crate) async fn original_language(
    ctx: &AppContext,
    item: &db::Media,
) -> Option<String> {
    if item
        .original_language
        .is_some()
        || item.kind != db::MediaKind::Episode
    {
        return item
            .original_language
            .clone();
    }
    let series_id = item.grandparent_id?;
    db::Media::get_by_id(&ctx.db, &series_id)
        .await
        .ok()??
        .original_language
}

fn language(raw: Option<&str>) -> Option<String> {
    raw.and_then(|s| {
        lang_to_two_letter(
            s.trim()
                .split(['-', '_'])
                .next()
                .unwrap_or(""),
        )
    })
}

fn preferred_language(raw: Option<&str>) -> bool {
    language(raw).is_some_and(|s| matches!(s.as_str(), "en" | "es"))
}

fn partial_subtitle_label(label: &str) -> bool {
    label
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| {
            [
                "forced", "sign", "signs", "song", "songs", "lyrics", "karaoke",
                "opening", "ending", "op", "ed",
            ]
            .iter()
            .any(|bad| word.eq_ignore_ascii_case(bad))
        })
}

/// An embedded, non-forced English/Spanish text subtitle for the dialogue.
fn full_subtitle(stream: &api::MediaStream) -> bool {
    stream.type_ == Some(MediaStreamType::Subtitle)
        && !stream.is_external
        && !stream.is_forced
        && preferred_language(
            stream
                .language
                .as_deref(),
        )
        && stream
            .subtitle_codec()
            .is_some_and(|codec| codec.is_text())
        && ![
            stream
                .title
                .as_deref(),
            stream
                .display_title
                .as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(partial_subtitle_label)
}

/// Append the source's languages to its name (`... | Audio: EN/JA | Subs: EN`)
/// for version pickers. Only a local ffprobe is trusted, as in `verdict`.
pub(crate) fn annotate_languages(source: &mut api::MediaSourceInfo) {
    let local_probe = source
        .remux
        .as_ref()
        .and_then(|remux| remux.source)
        == Some(api::ProbeOrigin::Ffprobe);
    if !local_probe {
        return;
    }
    let mut audio = Vec::new();
    let mut subtitles = Vec::new();
    for track in &source.media_streams {
        let languages = match track.type_ {
            Some(MediaStreamType::Audio) => &mut audio,
            Some(MediaStreamType::Subtitle) if full_subtitle(track) => &mut subtitles,
            _ => continue,
        };
        let Some(mut lang) = language(
            track
                .language
                .as_deref(),
        ) else {
            continue;
        };
        // The shared normalizer accepts arbitrary two-character tags.
        if isolang::Language::from_639_1(&lang).is_none() {
            continue;
        }
        lang.make_ascii_uppercase();
        languages.push(lang);
    }

    let mut suffix = String::new();
    for (label, mut languages) in [("Audio: ", audio), ("Subs: ", subtitles)] {
        languages.sort_unstable_by(|a, b| {
            (a != "EN", a != "ES", a).cmp(&(b != "EN", b != "ES", b))
        });
        languages.dedup();
        if languages.is_empty() {
            continue;
        }
        if !suffix.is_empty() {
            suffix.push_str(" | ");
        }
        suffix.push_str(label);
        suffix.push_str(&languages.join("/"));
    }
    if suffix.is_empty() {
        return;
    }
    let name = source
        .name
        .get_or_insert_with(String::new);
    if !name.ends_with(&suffix) {
        if !name.is_empty() {
            name.push_str(" | ");
        }
        name.push_str(&suffix);
    }
}

/// The audio (and, for original-language audio, subtitle) track indexes that
/// make `source` playable, preferring its current defaults.
pub(crate) fn qualifying_tracks(
    source: &api::MediaSourceInfo,
    original: Option<&str>,
) -> Option<(i64, Option<i64>)> {
    let audio = |s: &&api::MediaStream| s.type_ == Some(MediaStreamType::Audio);
    if let Some(track) = source
        .media_streams
        .iter()
        .filter(audio)
        .filter(|s| {
            preferred_language(
                s.language
                    .as_deref(),
            )
        })
        .min_by_key(|s| (Some(s.index) != source.default_audio_stream_index, s.index))
    {
        return Some((track.index, None));
    }
    let original = language(original)?;
    let track = source
        .media_streams
        .iter()
        .filter(audio)
        .find(|s| {
            language(
                s.language
                    .as_deref(),
            )
            .as_ref()
                == Some(&original)
        })?;
    let subtitle = source
        .media_streams
        .iter()
        .filter(|s| full_subtitle(s))
        .min_by_key(|s| {
            (
                Some(s.index) != source.default_subtitle_stream_index,
                s.index,
            )
        })?;
    Some((track.index, Some(subtitle.index)))
}

/// Make the qualifying tracks the defaults the client plays.
pub(crate) fn select_tracks(source: &mut api::MediaSourceInfo, original: Option<&str>) {
    if let Some((audio, subtitle)) = qualifying_tracks(source, original) {
        source.default_audio_stream_index = Some(audio);
        if subtitle.is_some() {
            source.default_subtitle_stream_index = subtitle;
        }
    }
}

/// The embedded subtitle `source` qualifies through for a strict user
/// (original-language audio plus this subtitle), if it qualifies that way.
/// Subtitle filters that would drop it in favor of another delivery path must
/// keep it, or the source loses the track that made it playable.
pub(crate) fn rescue_subtitle(
    source: &api::MediaSourceInfo,
    original: Option<&str>,
) -> Option<i64> {
    qualifying_tracks(source, original)?.1
}

/// Whether a strict user's source stopped qualifying once its embedded
/// subtitles were filtered down to what this request can deliver: it
/// qualified on the probed file (`qualified_before`) but no longer does. The
/// rescue subtitle is a text track, so it can only be delivered as Embed
/// (device profile) or External (extraction allowed); with neither, the
/// source is not playable for a strict user and must be rejected.
pub(crate) fn lost_qualification(
    qualified_before: bool,
    filtered: &api::MediaSourceInfo,
    original: Option<&str>,
) -> bool {
    qualified_before && qualifying_tracks(filtered, original).is_none()
}

#[derive(Debug, PartialEq)]
enum Verdict {
    Playable,
    Rejected,
    Unknown,
}

fn failure_key(id: Uuid) -> String {
    format!("language-gate:failed:{id}")
}

fn verdict(ctx: &AppContext, source: &db::Media, original: Option<&str>) -> Verdict {
    if ctx
        .store
        .get::<()>(failure_key(source.id))
        .is_some()
    {
        return Verdict::Rejected;
    }
    let local_probe = source
        .probe_data
        .as_ref()
        .filter(|probe| {
            probe
                .remux
                .as_ref()
                .and_then(|remux| remux.source)
                == Some(api::ProbeOrigin::Ffprobe)
        });
    match local_probe {
        Some(probe) if qualifying_tracks(probe, original).is_some() => {
            Verdict::Playable
        }
        Some(_) => Verdict::Rejected,
        None => Verdict::Unknown,
    }
}

/// Each source's autoplay rank: the index of the first visible, enabled stream
/// group it matches (groups in priority order), or `usize::MAX` otherwise.
async fn group_ranks(ctx: &AppContext, sources: &[db::Media]) -> Vec<usize> {
    let groups: Vec<db::StreamGroup> = db::StreamGroup::list(&ctx.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|g| g.enabled && !g.hidden)
        .collect();
    sources
        .iter()
        .map(|source| {
            groups
                .iter()
                .position(|group| {
                    source
                        .stream_info
                        .as_ref()
                        .is_some_and(|info| {
                            group.match_outcome(
                                info,
                                source
                                    .probe_data
                                    .as_ref(),
                            ) == db::MatchOutcome::Match
                        })
                })
                .unwrap_or(usize::MAX)
        })
        .collect()
}

fn running(probes: &HashMap<usize, Probe>) -> usize {
    probes
        .values()
        .filter(|&probe| {
            Probe::clone(probe)
                .now_or_never()
                .is_none()
        })
        .count()
}

/// Keep the sources a strict user may play, in their original order.
///
/// Walks sources in autoplay order (stream-group priority, then addon order)
/// until every group has a playable source, probing unknown ones at most
/// `PROBE_WINDOW` at a time; probes outlive the call and persist. The call
/// waits up to `PLAY_WAIT` only for the best-ranked source that is not
/// rejected, so a cold first play starts on the source later plays pick.
async fn retain_playable(
    ctx: &AppContext,
    mut sources: Vec<db::Media>,
    original: Option<&str>,
) -> Vec<db::Media> {
    let deadline = Instant::now() + PLAY_WAIT;
    let ranks = group_ranks(ctx, &sources).await;
    let mut order: Vec<usize> = (0..sources.len()).collect();
    order.sort_by_key(|&i| ranks[i]);
    let cfg = db::Settings::get_config_or_default(&ctx.db).await;
    let limit = |source: &db::Media| {
        let p2p = source
            .stream_info
            .as_ref()
            .is_some_and(|si| si.is_p2p());
        let secs = if p2p {
            cfg.probe_timeout_p2p_secs
                .unwrap_or(60)
        } else {
            cfg.probe_timeout_secs
                .unwrap_or(20)
        };
        Duration::from_secs(secs as u64)
    };

    let mut satisfied = HashSet::new();
    let mut waiting = true;
    let mut probes: HashMap<usize, Probe> = HashMap::new();
    for (pos, &i) in order
        .iter()
        .enumerate()
    {
        if satisfied.contains(&ranks[i]) {
            continue;
        }
        match verdict(ctx, &sources[i], original) {
            Verdict::Playable => {
                satisfied.insert(ranks[i]);
                waiting = false;
                continue;
            }
            Verdict::Rejected => continue,
            Verdict::Unknown => {}
        }
        for &j in &order[pos..] {
            if running(&probes) >= PROBE_WINDOW {
                break;
            }
            if !probes.contains_key(&j)
                && !satisfied.contains(&ranks[j])
                && verdict(ctx, &sources[j], original) == Verdict::Unknown
            {
                probes.insert(j, probe(ctx, &sources[j], limit(&sources[j])));
            }
        }
        if !waiting {
            continue;
        }
        let Some(probe) = probes
            .get(&i)
            .cloned()
        else {
            continue;
        };
        match tokio::time::timeout_at(deadline, probe).await {
            Ok(result) => {
                if result.is_some() {
                    sources[i].probe_data = result;
                }
                if verdict(ctx, &sources[i], original) == Verdict::Playable {
                    satisfied.insert(ranks[i]);
                    waiting = false;
                }
            }
            Err(_) => {
                debug!(id = %sources[i].id, "language probe still running at play deadline");
                waiting = false;
            }
        }
    }
    // Spare window slots learn the alternates of satisfied groups in the
    // background, so a group whose chosen file dies later still has siblings.
    for &j in &order {
        if running(&probes) >= PROBE_WINDOW {
            break;
        }
        if !probes.contains_key(&j)
            && verdict(ctx, &sources[j], original) == Verdict::Unknown
        {
            probes.insert(j, probe(ctx, &sources[j], limit(&sources[j])));
        }
    }
    for (&i, probe) in &probes {
        if let Some(Some(info)) = probe
            .clone()
            .now_or_never()
        {
            sources[i].probe_data = Some(info);
        }
    }
    sources.retain(|source| verdict(ctx, source, original) == Verdict::Playable);
    sources
}

/// Probe `source` once, however many requests ask; the result is persisted.
fn probe(ctx: &AppContext, source: &db::Media, limit: Duration) -> Probe {
    let mut in_flight = IN_FLIGHT
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(probe) = in_flight.get(&source.id) {
        return probe.clone();
    }
    let (ctx, source) = (ctx.clone(), source.clone());
    let id = source.id;
    let task = tokio::spawn(async move {
        let result = verify(&ctx, &source, limit).await;
        IN_FLIGHT
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        result
    });
    let probe = async move {
        task.await
            .ok()
            .flatten()
    }
    .boxed()
    .shared();
    in_flight.insert(id, probe.clone());
    probe
}

async fn verify(
    ctx: &AppContext,
    source: &db::Media,
    limit: Duration,
) -> Option<api::MediaSourceInfo> {
    let _permit = PROBES
        .acquire()
        .await
        .ok()?;
    let url = source
        .stream_info
        .as_ref()?
        .descriptor
        .server_input(
            source.id,
            ctx.config
                .port,
        );
    let probed = probe_media_bounded(&url, limit)
        .await
        .and_then(|(mut info, segments)| {
            if info
                .video_stream()
                .is_none()
                || info
                    .audio_stream()
                    .is_none()
            {
                return Err(anyhow!("missing video or audio"));
            }
            if placeholder_threshold(source, &info).is_some() {
                return Err(anyhow!("placeholder video"));
            }
            if !segments.is_empty() {
                info.segments = Some(segments);
            }
            Ok(info)
        });
    match probed {
        Ok(info) => {
            if let Err(error) =
                db::Media::save_probe_data(&ctx.db, &source.id, &info).await
            {
                debug!(id = %source.id, %error, "failed to save language probe");
            }
            Some(info)
        }
        Err(error) => {
            debug!(id = %source.id, %error, "language probe failed");
            ctx.store
                .save(failure_key(source.id), (), FAILURE_TTL);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(
        index: i64,
        kind: MediaStreamType,
        lang: &str,
        codec: &str,
    ) -> api::MediaStream {
        api::MediaStream {
            index,
            type_: Some(kind),
            language: Some(lang.into()),
            codec: Some(codec.into()),
            ..Default::default()
        }
    }

    fn source(streams: Vec<api::MediaStream>) -> api::MediaSourceInfo {
        api::MediaSourceInfo {
            media_streams: streams,
            ..Default::default()
        }
    }

    fn probed(audio: &str, origin: api::ProbeOrigin) -> db::Media {
        let mut info = source(vec![
            track(0, MediaStreamType::Video, "und", "h264"),
            track(1, MediaStreamType::Audio, audio, "aac"),
        ]);
        info.remux = Some(api::MediaSourceRemuxInfo {
            source: Some(origin),
            ..Default::default()
        });
        db::Media {
            id: Uuid::new_v4(),
            kind: db::MediaKind::Stream,
            probe_data: Some(info),
            ..Default::default()
        }
    }

    /// Only a local ffprobe is evidence: a filename guess or RemuxDB claim of
    /// English audio is re-verified, and a source that cannot be probed is
    /// never offered.
    #[tokio::test]
    async fn only_locally_probed_english_or_spanish_sources_are_playable() {
        let (_server, guard, _token) =
            crate::integration_test::authenticated_server().await;
        let guessed = probed("eng", api::ProbeOrigin::FilenameGuess);
        let russian = probed("rus", api::ProbeOrigin::Ffprobe);
        let english = probed("eng", api::ProbeOrigin::Ffprobe);
        let crowd = probed("spa", api::ProbeOrigin::RemuxDb);
        let english_id = english.id;

        let kept =
            retain_playable(&guard.0, vec![guessed, russian, english, crowd], None)
                .await;

        let ids: Vec<Uuid> = kept
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, [english_id]);
    }

    #[test]
    fn english_or_spanish_audio_qualifies_over_the_default_track() {
        let mut info = source(vec![
            track(0, MediaStreamType::Video, "und", "h264"),
            track(1, MediaStreamType::Audio, "rus", "aac"),
            track(2, MediaStreamType::Audio, "spa", "aac"),
        ]);
        info.default_audio_stream_index = Some(1);
        select_tracks(&mut info, Some("en"));
        assert_eq!(info.default_audio_stream_index, Some(2));
    }

    #[test]
    fn original_audio_needs_a_full_embedded_text_subtitle() {
        let video = track(0, MediaStreamType::Video, "und", "h264");
        let japanese = track(1, MediaStreamType::Audio, "jpn", "aac");
        let mut forced = track(2, MediaStreamType::Subtitle, "eng", "subrip");
        forced.is_forced = true;
        let image = track(3, MediaStreamType::Subtitle, "eng", "hdmv_pgs_subtitle");
        let mut signs = track(4, MediaStreamType::Subtitle, "eng", "ass");
        signs.title = Some("Signs & Songs".into());
        let full = track(5, MediaStreamType::Subtitle, "spa", "subrip");

        let partial =
            source(vec![video.clone(), japanese.clone(), forced, image, signs]);
        assert_eq!(qualifying_tracks(&partial, Some("ja")), None);

        let subtitled = source(vec![video, japanese, full]);
        assert_eq!(
            qualifying_tracks(&subtitled, Some("ja")),
            Some((1, Some(5)))
        );
        assert_eq!(
            qualifying_tracks(&subtitled, Some("ko")),
            None,
            "subtitles only rescue the title's original language"
        );
    }

    #[test]
    fn only_locally_probed_sources_are_labeled_with_their_languages() {
        let mut forced = track(4, MediaStreamType::Subtitle, "spa", "subrip");
        forced.is_forced = true;
        let mut signs = track(5, MediaStreamType::Subtitle, "spa", "ass");
        signs.title = Some("Signs & Songs".into());
        let mut info = source(vec![
            track(0, MediaStreamType::Video, "und", "h264"),
            track(1, MediaStreamType::Audio, "jpn", "aac"),
            track(2, MediaStreamType::Audio, "spa", "aac"),
            track(3, MediaStreamType::Audio, "eng", "aac"),
            track(6, MediaStreamType::Audio, "en-US", "aac"),
            forced,
            signs,
            track(7, MediaStreamType::Subtitle, "spa", "hdmv_pgs_subtitle"),
            track(8, MediaStreamType::Subtitle, "eng", "subrip"),
        ]);
        info.name = Some("4K".into());
        let mut crowd = info.clone();
        crowd.remux = Some(api::MediaSourceRemuxInfo {
            source: Some(api::ProbeOrigin::RemuxDb),
            ..Default::default()
        });
        let mut unprobed = info.clone();
        info.remux = Some(api::MediaSourceRemuxInfo {
            source: Some(api::ProbeOrigin::Ffprobe),
            ..Default::default()
        });

        for _ in 0..2 {
            annotate_languages(&mut info);
            annotate_languages(&mut crowd);
            annotate_languages(&mut unprobed);
        }
        assert_eq!(
            info.name
                .as_deref(),
            Some("4K | Audio: EN/ES/JA | Subs: EN")
        );
        assert_eq!(
            crowd
                .name
                .as_deref(),
            Some("4K")
        );
        assert_eq!(
            unprobed
                .name
                .as_deref(),
            Some("4K")
        );
    }
}
