use anyhow::Result;
use chrono::Utc;
use remux_sdks::remux::{
    FilterMatchMode, NumericOp, SetOp, StreamCodec, StreamFilter, StreamQuality,
    StreamResolution, StreamRule, common_audio_languages, format_size_rule,
    language_label, normalize_lang_code,
};
use sqlx::SqlitePool;
use std::collections::HashSet;
use uuid::Uuid;

use crate::{
    api::MediaStreamType,
    db::{Media, Settings, StreamGroupData},
    stream::StreamInfo,
};

const LEGACY_SETTINGS_KEY: &str = "remux:stream_groups";

#[derive(Debug, Clone)]
pub struct StreamGroup {
    pub id: Uuid,
    pub name: String,
    pub filter: StreamFilter,
    pub priority: i64,
    pub enabled: bool,
    pub hidden: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchOutcome {
    Match,
    PassThrough,
    NoMatch,
}

fn bool_to_outcome(hit: bool) -> MatchOutcome {
    if hit {
        MatchOutcome::Match
    } else {
        MatchOutcome::NoMatch
    }
}

impl StreamGroup {
    pub fn display_name(&self) -> String {
        if !self
            .name
            .is_empty()
        {
            return self
                .name
                .clone();
        }
        auto_name(&self.filter)
    }

    fn from_media(m: Media) -> Option<Self> {
        let data = m.stream_group_data?;
        Some(Self {
            id: m.id,
            name: data.name,
            filter: data.filter,
            priority: data.priority,
            enabled: m.enabled,
            hidden: data.hidden,
            created_at: data.created_at,
        })
    }

    async fn save(&self, db: &SqlitePool) -> Result<()> {
        let data = StreamGroupData {
            name: self
                .name
                .clone(),
            filter: self
                .filter
                .clone(),
            priority: self.priority,
            hidden: self.hidden,
            created_at: self
                .created_at
                .clone(),
        };
        let data_json = serde_json::to_string(&data)?;
        let now = Utc::now().naive_utc();
        sqlx::query(
            "INSERT INTO media (id, kind, title, enabled, stream_group_data, external_ids, locked_fields, created_at, updated_at)
             VALUES (?, 'stream_group', ?, ?, ?, '{}', '[]', ?, ?)
             ON CONFLICT (id) DO UPDATE SET
                 title = excluded.title,
                 enabled = excluded.enabled,
                 stream_group_data = excluded.stream_group_data,
                 external_ids = COALESCE(media.external_ids, excluded.external_ids),
                 locked_fields = COALESCE(media.locked_fields, '[]'),
                 updated_at = excluded.updated_at",
        )
        .bind(self.id)
        .bind(self.display_name())
        .bind(self.enabled)
        .bind(&data_json)
        .bind(now)
        .bind(now)
        .execute(db)
        .await?;
        Ok(())
    }

    /// One-time migration: reads groups from the legacy settings key and saves them
    /// as media rows. After migration, removes the settings key.
    pub async fn migrate_from_settings(db: &SqlitePool) {
        #[derive(serde::Deserialize)]
        struct Legacy {
            id: Uuid,
            #[serde(default)]
            name: String,
            filter: StreamFilter,
            #[serde(default)]
            priority: i64,
            #[serde(default = "default_true")]
            enabled: bool,
            #[serde(default)]
            hidden: bool,
            #[serde(default)]
            created_at: String,
        }
        fn default_true() -> bool {
            true
        }

        let json = match Settings::get(db, LEGACY_SETTINGS_KEY).await {
            Ok(Some(j)) => j,
            _ => return,
        };
        let legacy: Vec<Legacy> = match serde_json::from_str(&json) {
            Ok(v) => v,
            Err(_) => return,
        };
        for l in legacy {
            let group = StreamGroup {
                id: l.id,
                name: l.name,
                filter: l.filter,
                priority: l.priority,
                enabled: l.enabled,
                hidden: l.hidden,
                created_at: l.created_at,
            };
            let _ = group
                .save(db)
                .await;
        }
        let _ = sqlx::query("DELETE FROM settings WHERE key = ?")
            .bind(LEGACY_SETTINGS_KEY)
            .execute(db)
            .await;
    }

    pub async fn list(db: &SqlitePool) -> Result<Vec<Self>> {
        let rows: Vec<Media> =
            sqlx::query_as("SELECT * FROM media WHERE kind = 'stream_group'")
                .fetch_all(db)
                .await?;
        let mut groups: Vec<Self> = rows
            .into_iter()
            .filter_map(Self::from_media)
            .collect();
        groups.sort_by_key(|g| g.priority);
        Ok(groups)
    }

    pub async fn get_by_id(db: &SqlitePool, id: &Uuid) -> Result<Option<Self>> {
        let row: Option<Media> = sqlx::query_as(
            "SELECT * FROM media WHERE id = ? AND kind = 'stream_group'",
        )
        .bind(id)
        .fetch_optional(db)
        .await?;
        Ok(row.and_then(Self::from_media))
    }

    pub async fn create(
        db: &SqlitePool,
        name: &str,
        filter: StreamFilter,
        priority: i64,
    ) -> Result<Self> {
        let group = Self {
            id: Uuid::new_v4(),
            name: name.to_string(),
            filter,
            priority,
            enabled: true,
            hidden: false,
            created_at: Utc::now()
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        };
        group
            .save(db)
            .await?;
        Ok(group)
    }

    pub async fn update(
        db: &SqlitePool,
        id: &Uuid,
        name: &str,
        filter: StreamFilter,
        priority: i64,
        enabled: bool,
        hidden: bool,
    ) -> Result<Self> {
        let existing = Self::get_by_id(db, id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("stream group not found"))?;
        let group = Self {
            id: *id,
            name: name.to_string(),
            filter,
            priority,
            enabled,
            hidden,
            created_at: existing.created_at,
        };
        group
            .save(db)
            .await?;
        Ok(group)
    }

    pub async fn delete(db: &SqlitePool, id: &Uuid) -> Result<()> {
        Media::delete(db, id).await
    }

    /// Filter a list of media sources by the enabled stream groups.
    ///
    /// Returns one representative source per matching group — the best
    /// candidate on the quality ladder (see [`rank_by_quality`]), ties broken
    /// by incoming order — plus any unmatched sources when `show_ungrouped`
    /// is true.  When no groups are enabled the original list is returned
    /// unchanged.
    pub async fn filter_sources(
        db: &SqlitePool,
        sources: Vec<Media>,
        show_ungrouped: bool,
    ) -> Vec<Media> {
        let groups = match Self::list(db).await {
            Ok(groups) => groups,
            Err(_) => return sources,
        };
        Self::filter_sources_for_groups(&groups, sources, show_ungrouped)
    }

    pub(crate) fn filter_sources_for_groups(
        groups: &[StreamGroup],
        sources: Vec<Media>,
        show_ungrouped: bool,
    ) -> Vec<Media> {
        let mut result = Vec::new();
        let mut matched_ids = HashSet::new();
        let mut any_enabled = false;

        for group in groups
            .iter()
            .filter(|group| group.enabled)
        {
            any_enabled = true;
            let mut matching: Vec<&Media> = sources
                .iter()
                .filter(|source| {
                    source
                        .stream_info
                        .as_ref()
                        .is_some_and(|info| {
                            group.match_outcome_ctx(
                                info,
                                source
                                    .probe_data
                                    .as_ref(),
                                source
                                    .original_language
                                    .as_deref(),
                            ) == MatchOutcome::Match
                        })
                })
                .collect();
            if matching.is_empty() {
                continue;
            }

            matched_ids.extend(
                matching
                    .iter()
                    .map(|source| source.id),
            );
            if !group.hidden {
                // group_id marks the representative while its real stream UUID
                // stays in id, keeping internal probe URLs correct.
                rank_by_quality(&mut matching);
                let mut best = matching[0].clone();
                best.title = group.display_name();
                best.group_id = Some(group.id);
                result.push(best);
            }
        }

        if !any_enabled {
            return sources;
        }
        if show_ungrouped {
            result.extend(
                sources
                    .into_iter()
                    .filter(|source| !matched_ids.contains(&source.id)),
            );
        }
        result
    }
}

/// Filter a list of source Media items using a `StreamFilter`.
/// Sources without `stream_info` (unparseable filename) are kept unchanged.
/// An empty filter (no rules) is a no-op.
pub fn apply_stream_filter(filter: &StreamFilter, sources: Vec<Media>) -> Vec<Media> {
    if filter
        .rules
        .is_empty()
    {
        return sources;
    }
    let temp = StreamGroup {
        id: Uuid::nil(),
        name: String::new(),
        filter: filter.clone(),
        priority: 0,
        enabled: true,
        hidden: false,
        created_at: String::new(),
    };
    sources
        .into_iter()
        .filter(|s| {
            s.stream_info
                .as_ref()
                .map_or(true, |info| {
                    temp.match_outcome_ctx(
                        info,
                        s.probe_data
                            .as_ref(),
                        s.original_language
                            .as_deref(),
                    ) != MatchOutcome::NoMatch
                })
        })
        .collect()
}

impl StreamGroup {
    pub fn match_outcome(
        &self,
        info: &StreamInfo,
        probe_data: Option<&crate::api::MediaSourceInfo>,
    ) -> MatchOutcome {
        self.match_outcome_ctx(info, probe_data, None)
    }

    /// Like [`Self::match_outcome`], with the title's effective original
    /// language (BCP 47 or ISO 639) as last-resort AudioLanguage evidence
    /// (see [`language_evidence`]).
    pub fn match_outcome_ctx(
        &self,
        info: &StreamInfo,
        probe_data: Option<&crate::api::MediaSourceInfo>,
        original_language: Option<&str>,
    ) -> MatchOutcome {
        let filter = &self.filter;
        if filter
            .rules
            .is_empty()
        {
            return MatchOutcome::Match;
        }

        let raw = match info
            .filename
            .as_deref()
            .or(info
                .name
                .as_deref())
        {
            Some(s) => s,
            None => return MatchOutcome::PassThrough,
        };

        let parsed = match best_parse(raw) {
            Some(p) => p,
            None => return MatchOutcome::PassThrough,
        };

        let resolution = min_screen_size(&parsed)
            .and_then(StreamResolution::from_hunch)
            .unwrap_or(StreamResolution::Other);
        let source = source_of(&parsed, raw);
        let codec = parsed
            .video_codec()
            .and_then(StreamCodec::from_hunch)
            .unwrap_or(StreamCodec::Other);

        let eval = |rule: &StreamRule| -> MatchOutcome {
            match rule {
                StreamRule::Resolution { op, values } => {
                    if resolution == StreamResolution::Other
                        && !values.contains(&StreamResolution::Other)
                    {
                        return MatchOutcome::PassThrough;
                    }
                    let hit = values.contains(&resolution);
                    bool_to_outcome(matches!(op, SetOp::In | SetOp::Is) == hit)
                }
                StreamRule::Quality { op, values } => {
                    if source == StreamQuality::Other
                        && !values.contains(&StreamQuality::Other)
                    {
                        return MatchOutcome::PassThrough;
                    }
                    let hit = values.contains(&source);
                    bool_to_outcome(matches!(op, SetOp::In | SetOp::Is) == hit)
                }
                StreamRule::Codec { op, values } => {
                    if codec == StreamCodec::Other
                        && !values.contains(&StreamCodec::Other)
                    {
                        return MatchOutcome::PassThrough;
                    }
                    let hit = values.contains(&codec);
                    bool_to_outcome(matches!(op, SetOp::In | SetOp::Is) == hit)
                }
                // Evidence precedence: concrete probed audio tracks →
                // filename (hunch languages + release tags) → title original
                // language. No evidence at any tier counts as "not a desired
                // language": In/Is fails and NotIn/IsNot matches, so
                // complementary In/NotIn groups over the same codes partition
                // sources deterministically.
                StreamRule::AudioLanguage { op, values } => {
                    let evidence =
                        language_evidence(probe_data, &parsed, raw, original_language);
                    let wanted: Vec<String> = values
                        .iter()
                        .filter_map(|v| evidence_lang_code(v))
                        .collect();
                    let hit = evidence
                        .iter()
                        .any(|l| wanted.contains(l));
                    bool_to_outcome(matches!(op, SetOp::In | SetOp::Is) == hit)
                }
                // Unknown size (None) passes through so HTTP/debrid/IPTV
                // sources are not silently dropped.
                StreamRule::Size { op, value } => match info.size {
                    None => MatchOutcome::PassThrough,
                    Some(s) => bool_to_outcome(match op {
                        NumericOp::Eq => s == *value,
                        NumericOp::NotEq => s != *value,
                        NumericOp::Gt => s > *value,
                        NumericOp::Lt => s < *value,
                    }),
                },
            }
        };

        let outcomes: Vec<MatchOutcome> = filter
            .rules
            .iter()
            .map(eval)
            .collect();
        match filter.match_mode {
            FilterMatchMode::All => {
                if outcomes
                    .iter()
                    .any(|o| matches!(o, MatchOutcome::NoMatch))
                {
                    MatchOutcome::NoMatch
                } else if outcomes
                    .iter()
                    .any(|o| matches!(o, MatchOutcome::PassThrough))
                {
                    MatchOutcome::PassThrough
                } else {
                    MatchOutcome::Match
                }
            }
            FilterMatchMode::Any => {
                if outcomes
                    .iter()
                    .any(|o| matches!(o, MatchOutcome::Match))
                {
                    MatchOutcome::Match
                } else {
                    MatchOutcome::NoMatch
                }
            }
        }
    }

    fn candidates_for_group(group: &StreamGroup, raw_sources: &[Media]) -> Vec<Media> {
        let mut v: Vec<Media> = raw_sources
            .iter()
            .filter(|s| {
                s.stream_info
                    .as_ref()
                    .map_or(false, |info| {
                        group.match_outcome_ctx(
                            info,
                            s.probe_data
                                .as_ref(),
                            s.original_language
                                .as_deref(),
                        ) == MatchOutcome::Match
                    })
            })
            .cloned()
            .collect();
        v.sort_by_key(|s| {
            s.idx
                .unwrap_or(0)
        });
        rank_by_quality(&mut v);
        v
    }

    /// Loads the raw stream rows for `parent_id`, falling back to the parent
    /// itself when it carries its own stream (locally-imported files).
    async fn raw_streams(db: &SqlitePool, parent_id: &Uuid) -> Result<Vec<Media>> {
        let mut parent = Media::get_by_id(db, parent_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("parent item not found"))?;
        let raw = parent
            .streams(db)
            .await?;
        if !raw.is_empty() {
            return Ok(raw);
        }
        // The parent doubles as its own source; give it the same effective
        // language its stream rows would inherit from `Media::streams`.
        if parent
            .original_language
            .is_none()
        {
            parent.original_language = parent
                .effective_original_language(db)
                .await;
        }
        Ok(vec![parent])
    }

    /// Returns all raw sources matching this group for the given parent item,
    /// quality-ranked. Used by PlaybackInfo for group-scoped probe fallback.
    pub async fn streams_for(
        db: &SqlitePool,
        group_id: &Uuid,
        parent_id: &Uuid,
    ) -> Result<Vec<Media>> {
        let group = Self::get_by_id(db, group_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("stream group not found"))?;
        let raw = Self::raw_streams(db, parent_id).await?;
        Ok(Self::candidates_for_group(&group, &raw))
    }

    /// Returns streams from all enabled groups that come after `current_group_id`
    /// in priority order, concatenated. Used to extend the probe fallback pool
    /// so that exhausting one group cascades into the next.
    pub async fn streams_for_groups_after(
        db: &SqlitePool,
        current_group_id: &Uuid,
        parent_id: &Uuid,
    ) -> Result<Vec<Media>> {
        let groups = Self::list(db).await?;
        let raw = Self::raw_streams(db, parent_id).await?;

        let mut result = vec![];
        let mut found = false;
        for group in groups
            .iter()
            .filter(|g| g.enabled)
        {
            if !found {
                if group.id == *current_group_id {
                    found = true;
                }
                continue;
            }
            result.extend(Self::candidates_for_group(group, &raw));
        }
        Ok(result)
    }
}

/// Parses the best hunch candidate from a (possibly multi-line) stream name:
/// torrent titles often carry a display line plus a filename line; the line
/// yielding the most parsed facts wins. `None` when there are no non-empty
/// lines.
fn best_parse(raw: &str) -> Option<hunch::HunchResult> {
    raw.lines()
        .filter(|line| {
            !line
                .trim()
                .is_empty()
        })
        .map(hunch::hunch)
        .max_by_key(|parsed| {
            (parsed
                .screen_size()
                .is_some() as u8)
                + (parsed
                    .source()
                    .is_some() as u8)
        })
}

/// Source quality of a parsed stream name, with substring fallback when
/// hunch fails to identify the source.
fn source_of(parsed: &hunch::HunchResult, raw: &str) -> StreamQuality {
    let s = canonical_source(parsed);
    if s == StreamQuality::Other {
        fallback_source(raw)
    } else {
        s
    }
}

/// Audio-language evidence for one source, by precedence: concrete probed
/// audio-track languages override filename evidence (hunch languages plus
/// release tags), which overrides the title's effective original language.
/// Codes are normalized via [`evidence_lang_code`]; empty = no evidence at
/// any tier.
fn language_evidence(
    probe_data: Option<&crate::api::MediaSourceInfo>,
    parsed: &hunch::HunchResult,
    raw: &str,
    original_language: Option<&str>,
) -> Vec<String> {
    let probed: Vec<String> = probe_data
        .into_iter()
        .flat_map(|pd| {
            pd.media_streams
                .iter()
        })
        .filter(|s| s.type_ == Some(MediaStreamType::Audio))
        .filter_map(|s| {
            s.language
                .as_deref()
        })
        .filter_map(evidence_lang_code)
        .collect();
    if !probed.is_empty() {
        return probed;
    }

    let mut filename: Vec<String> = parsed
        .languages()
        .iter()
        .filter_map(|l| evidence_lang_code(l))
        .collect();
    if let Some(castellano) = spanish_release_tag(raw) {
        if castellano {
            // hunch 2.0.1 maps Castellano to Catalan; the release term means
            // Castilian Spanish.
            filename.retain(|language| language != "cat");
        }
        if !filename
            .iter()
            .any(|language| language == "spa")
        {
            filename.push("spa".to_string());
        }
    }
    if !filename.is_empty() {
        return filename;
    }

    original_language
        .and_then(evidence_lang_code)
        .into_iter()
        .collect()
}

/// Normalizes BCP 47, ISO 639, and hunch display names to the ISO 639-2
/// codes used by `StreamRule::AudioLanguage`. Special no-content codes are
/// not language evidence; unknown values remain concrete but cannot match a
/// known code.
fn evidence_lang_code(raw: &str) -> Option<String> {
    let lower = raw
        .trim()
        .to_ascii_lowercase();
    let primary = lower
        .split(['-', '_'])
        .next()
        .unwrap_or("");
    if primary.is_empty() || matches!(primary, "und" | "mis" | "zxx" | "mul") {
        return None;
    }
    if let Some((code, _)) = common_audio_languages()
        .iter()
        .find(|(code, name)| {
            code.eq_ignore_ascii_case(primary) || name.eq_ignore_ascii_case(primary)
        })
    {
        return Some((*code).to_string());
    }
    let language = isolang::Language::from_639_1(primary)
        .or_else(|| isolang::Language::from_639_3(primary))
        .or_else(|| {
            isolang::languages().find(|language| {
                language
                    .to_name()
                    .eq_ignore_ascii_case(primary)
            })
        });
    Some(
        language
            .map(|language| normalize_lang_code(language.to_639_3()))
            .unwrap_or_else(|| normalize_lang_code(primary)),
    )
}

/// Separator-bounded Spanish release tags not handled correctly by hunch.
/// A neighboring subtitle marker suppresses the inference: `Sub.Lat` says
/// nothing about the audio track. The return value identifies Castellano so
/// its incorrect hunch Catalan classification can be removed.
fn spanish_release_tag(raw: &str) -> Option<bool> {
    let is_subtitle = |token: &str| {
        token.eq_ignore_ascii_case("sub")
            || token.eq_ignore_ascii_case("subs")
            || token.eq_ignore_ascii_case("subtitle")
            || token.eq_ignore_ascii_case("subtitles")
    };
    let mut tokens = raw
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .peekable();
    let mut previous = None;
    while let Some(token) = tokens.next() {
        let castellano = token.eq_ignore_ascii_case("castellano");
        let is_spanish = castellano
            || token.eq_ignore_ascii_case("lat")
            || token.eq_ignore_ascii_case("latam");
        let subtitle_context = previous.is_some_and(is_subtitle)
            || tokens
                .peek()
                .is_some_and(|next| is_subtitle(next));
        if is_spanish && !subtitle_context {
            return Some(castellano);
        }
        previous = Some(token);
    }
    None
}

/// Ranks group candidates in place by resolution, then source quality:
/// 2160p > 1080p > 720p > 480p > 360p > Unknown; within a resolution,
/// Blu-ray Remux > Blu-ray > WEB-DL > WEBRip > HDTV > DVD > TV > Unknown.
/// The sort is stable, so equal candidates keep incoming (raw/idx) order.
/// Shared by `filter_sources`, group failover, and preview.
pub(crate) fn rank_by_quality<M: std::borrow::Borrow<Media>>(candidates: &mut [M]) {
    candidates.sort_by_cached_key(|media| {
        M::borrow(media)
            .stream_info
            .as_ref()
            .map_or((5, 7), candidate_rank)
    });
}

fn candidate_rank(info: &StreamInfo) -> (u8, u8) {
    let Some(raw) = info
        .filename
        .as_deref()
        .or(info
            .name
            .as_deref())
    else {
        return (5, 7);
    };
    let Some(parsed) = best_parse(raw) else {
        return (5, 7);
    };
    let resolution = min_screen_size(&parsed)
        .and_then(StreamResolution::from_hunch)
        .unwrap_or(StreamResolution::Other);
    let resolution_rank = match resolution {
        StreamResolution::R2160p => 0,
        StreamResolution::R1080p => 1,
        StreamResolution::R720p => 2,
        StreamResolution::R480p => 3,
        StreamResolution::R360p => 4,
        StreamResolution::Other => 5,
    };
    let quality_rank = match source_of(&parsed, raw) {
        StreamQuality::BluRayRemux => 0,
        StreamQuality::BluRay => 1,
        StreamQuality::WebDl => 2,
        StreamQuality::WebRip => 3,
        StreamQuality::Hdtv => 4,
        StreamQuality::Dvd => 5,
        StreamQuality::Tv => 6,
        StreamQuality::Other => 7,
    };
    (resolution_rank, quality_rank)
}

/// Picks the lowest of hunch's screen-size candidates.
///
/// Filenames like "UHD BluRay 1080p ... x265" parse to multiple screen_size
/// hits (hunch maps the bare word "UHD" to 2160p in addition to the literal
/// 1080p token) because "UHD BluRay" credits the source disc, not the encode
/// resolution. Encoders only ever downscale from source, never label a
/// downscaled encode with a higher resolution, so the smaller candidate is
/// always the real encode resolution when they disagree.
pub(crate) fn min_screen_size<'a>(parsed: &'a hunch::HunchResult) -> Option<&'a str> {
    parsed
        .all(hunch::Property::ScreenSize)
        .into_iter()
        .min_by_key(|s| {
            s.trim_end_matches(['p', 'i'])
                .parse::<u32>()
                .unwrap_or(u32::MAX)
        })
}

fn auto_name(filter: &StreamFilter) -> String {
    let parts: Vec<String> = filter
        .rules
        .iter()
        .filter_map(|r| {
            let labels: Vec<String> = match r {
                StreamRule::Resolution { values, .. } => values
                    .iter()
                    .map(|v| v.label())
                    .map(str::to_owned)
                    .collect(),
                StreamRule::Quality { values, .. } => values
                    .iter()
                    .map(|v| v.label())
                    .map(str::to_owned)
                    .collect(),
                StreamRule::Codec { values, .. } => values
                    .iter()
                    .map(|v| v.label())
                    .map(str::to_owned)
                    .collect(),
                StreamRule::AudioLanguage { values, .. } => values
                    .iter()
                    .map(|c| language_label(c))
                    .collect(),
                StreamRule::Size { op, value } => vec![format_size_rule(*op, *value)],
            };
            if labels.is_empty() {
                None
            } else {
                Some(labels.join("/"))
            }
        })
        .collect();
    if parts.is_empty() {
        "All streams".to_string()
    } else {
        parts.join(" · ")
    }
}

fn canonical_source(parsed: &hunch::HunchResult) -> StreamQuality {
    let other = parsed.other();
    let is_remux = other.contains(&"Remux");
    let is_rip = other.contains(&"Rip");
    let Some(source) = parsed.source() else {
        // No source keyword — remux is always Blu-ray regardless
        return if is_remux {
            StreamQuality::BluRayRemux
        } else {
            StreamQuality::Other
        };
    };
    match source {
        "Web" if is_rip => StreamQuality::WebRip,
        "Web" => StreamQuality::WebDl,
        "Blu-ray" | "Ultra HD Blu-ray" if is_remux => StreamQuality::BluRayRemux,
        "Blu-ray" | "Ultra HD Blu-ray" => StreamQuality::BluRay,
        "HDTV" => StreamQuality::Hdtv,
        "DVD" => StreamQuality::Dvd,
        "TV" => StreamQuality::Tv,
        _ => StreamQuality::Other,
    }
}

/// Fallback source detection via case-insensitive substring search.
/// Used when hunch fails to identify the source (e.g. "WEBRip-AVC" or
/// space-delimited multi-language filenames where hunch loses the token).
fn fallback_source(raw: &str) -> StreamQuality {
    let lower = raw.to_lowercase();
    if lower.contains("webrip") || lower.contains("web-rip") {
        StreamQuality::WebRip
    } else if lower.contains("web-dl") || lower.contains("webdl") {
        StreamQuality::WebDl
    } else if lower.contains("remux") {
        StreamQuality::BluRayRemux
    } else if lower.contains("bluray") || lower.contains("blu-ray") {
        StreamQuality::BluRay
    } else if lower.contains("hdtv") {
        StreamQuality::Hdtv
    } else if lower.contains("dvdrip") {
        StreamQuality::Dvd
    } else {
        StreamQuality::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::StreamDescriptor;

    fn group_1080p_bluray() -> StreamGroup {
        StreamGroup {
            id: Uuid::nil(),
            name: "1080p · Blu-ray".to_string(),
            filter: StreamFilter {
                match_mode: FilterMatchMode::All,
                rules: vec![
                    StreamRule::Resolution {
                        op: SetOp::In,
                        values: vec![StreamResolution::R1080p],
                    },
                    StreamRule::Quality {
                        op: SetOp::In,
                        values: vec![StreamQuality::BluRay, StreamQuality::BluRayRemux],
                    },
                ],
            },
            priority: 0,
            enabled: true,
            hidden: false,
            created_at: String::new(),
        }
    }

    fn info(filename: &str) -> StreamInfo {
        StreamInfo {
            descriptor: StreamDescriptor::default(),
            filename: Some(filename.to_string()),
            ..Default::default()
        }
    }

    // "UHD BluRay" credits the source disc; the explicit "1080p" token is the
    // real encode resolution. These are real filenames that were falling
    // through to Ungrouped before min_screen_size() was introduced.
    #[test]
    fn uhd_bluray_source_with_explicit_1080p_matches_1080p_group() {
        let group = group_1080p_bluray();
        assert_eq!(
            group.match_outcome(
                &info(
                    "The Martian 2015 Extended Cut UHD BluRay 1080p DD Atmos 5 1 DoVi HDR10 x265-SM737.mkv"
                ),
                None
            ),
            MatchOutcome::Match
        );
        assert_eq!(
            group.match_outcome(
                &info(
                    "The.Housemaid.2025.UHD.BluRay.1080p.DD+Atmos.5.1.DoVi.HDR10+.x265-SM737.mkv"
                ),
                None
            ),
            MatchOutcome::Match
        );
        assert_eq!(
            group.match_outcome(
                &info(
                    "The Housemaid 2025 REPACK UHD BluRay 1080p DD Atmos 5 1 DoVi HDR10 x265-SM737.mkv"
                ),
                None
            ),
            MatchOutcome::Match
        );
    }

    #[test]
    fn genuine_2160p_release_does_not_match_1080p_group() {
        let group = group_1080p_bluray();
        assert_eq!(
            group.match_outcome(
                &info(
                    "The Martian 2015 UHD BluRay 2160p HDR10 DoVi Atmos x265-GROUP.mkv"
                ),
                None
            ),
            MatchOutcome::NoMatch
        );
    }

    fn group_audio_lang(op: SetOp, values: &[&str]) -> StreamGroup {
        StreamGroup {
            id: Uuid::nil(),
            name: "audio".to_string(),
            filter: StreamFilter {
                match_mode: FilterMatchMode::All,
                rules: vec![StreamRule::AudioLanguage {
                    op,
                    values: values
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                }],
            },
            priority: 0,
            enabled: true,
            hidden: false,
            created_at: String::new(),
        }
    }

    fn probe_with_langs(langs: &[&str]) -> crate::api::MediaSourceInfo {
        use crate::api::MediaStream;
        let media_streams = langs
            .iter()
            .map(|l| MediaStream {
                type_: Some(MediaStreamType::Audio),
                language: Some(l.to_string()),
                ..Default::default()
            })
            .collect();
        crate::api::MediaSourceInfo {
            media_streams,
            ..Default::default()
        }
    }

    #[test]
    fn audio_lang_matches_when_listed() {
        let group = group_audio_lang(SetOp::Is, &["rus"]);
        let probe = probe_with_langs(&["rus"]);
        assert_eq!(
            group.match_outcome(&info("a.mkv"), Some(&probe)),
            MatchOutcome::Match
        );
    }

    #[test]
    fn audio_lang_rejects_when_language_differs() {
        let group = group_audio_lang(SetOp::Is, &["rus"]);
        let probe = probe_with_langs(&["eng"]);
        assert_eq!(
            group.match_outcome(&info("a.mkv"), Some(&probe)),
            MatchOutcome::NoMatch
        );
    }

    #[test]
    fn audio_lang_case_insensitive_and_any_track() {
        let group = group_audio_lang(SetOp::In, &["rus"]);
        assert_eq!(
            group.match_outcome(&info("a.mkv"), Some(&probe_with_langs(&["RUS"]))),
            MatchOutcome::Match
        );
        assert_eq!(
            group.match_outcome(
                &info("a.mkv"),
                Some(&probe_with_langs(&["eng", "rus"]))
            ),
            MatchOutcome::Match
        );
    }

    #[test]
    fn audio_lang_isnot_inverts() {
        let group = group_audio_lang(SetOp::NotIn, &["rus"]);
        assert_eq!(
            group.match_outcome(&info("a.mkv"), Some(&probe_with_langs(&["rus"]))),
            MatchOutcome::NoMatch
        );
        assert_eq!(
            group.match_outcome(&info("a.mkv"), Some(&probe_with_langs(&["eng"]))),
            MatchOutcome::Match
        );
    }

    // With no probe, no filename language, and no title language there is no
    // evidence at any tier: desired-language membership is false, so In/Is
    // fails and NotIn/IsNot matches. Complementary In/NotIn groups over the
    // same codes therefore partition every source deterministically —
    // fallback groups catch what desired-language groups reject.
    #[test]
    fn audio_lang_no_evidence_fails_in_and_matches_not_in() {
        let f = info("Movie.2024.1080p.WEB-DL.mkv");
        assert_eq!(
            group_audio_lang(SetOp::In, &["eng", "spa"]).match_outcome(&f, None),
            MatchOutcome::NoMatch
        );
        assert_eq!(
            group_audio_lang(SetOp::Is, &["eng"]).match_outcome(&f, None),
            MatchOutcome::NoMatch
        );
        assert_eq!(
            group_audio_lang(SetOp::NotIn, &["eng", "spa"]).match_outcome(&f, None),
            MatchOutcome::Match
        );
        assert_eq!(
            group_audio_lang(SetOp::IsNot, &["eng"]).match_outcome(&f, None),
            MatchOutcome::Match
        );
    }

    // Special "no-linguistic-content" codes are not concrete evidence; the
    // probe tier falls through to the lower tiers, which are empty here.
    #[test]
    fn audio_lang_ignores_special_codes() {
        let group = group_audio_lang(SetOp::Is, &["rus"]);
        assert_eq!(
            group.match_outcome(&info("a.mkv"), Some(&probe_with_langs(&["und"]))),
            MatchOutcome::NoMatch
        );
        assert_eq!(
            group.match_outcome(&info("a.mkv"), Some(&probe_with_langs(&["mul"]))),
            MatchOutcome::NoMatch
        );
    }

    #[test]
    fn resolution_group_passes_through_source_without_filename() {
        let group = StreamGroup {
            id: Uuid::nil(),
            name: "1080p".to_string(),
            filter: StreamFilter {
                match_mode: FilterMatchMode::All,
                rules: vec![StreamRule::Resolution {
                    op: SetOp::In,
                    values: vec![StreamResolution::R1080p],
                }],
            },
            priority: 0,
            enabled: true,
            hidden: false,
            created_at: String::new(),
        };
        let si = StreamInfo {
            descriptor: StreamDescriptor::default(),
            filename: None,
            name: None,
            ..Default::default()
        };
        assert_eq!(group.match_outcome(&si, None), MatchOutcome::PassThrough);
    }

    // ── Language evidence ───────────────────────────────────────────────

    fn group_eng_spa() -> StreamGroup {
        group_audio_lang(SetOp::In, &["eng", "spa"])
    }

    #[test]
    fn audio_lang_hunch_filename_languages_match() {
        let group = group_eng_spa();
        assert_eq!(
            group.match_outcome(&info("Movie.2024.1080p.WEBRip.Spanish.mkv"), None),
            MatchOutcome::Match
        );
        assert_eq!(
            group.match_outcome(&info("Movie.2024.1080p.English.WEB-DL.mkv"), None),
            MatchOutcome::Match
        );
        // An explicit non-preferred filename language is concrete evidence,
        // not an unknown: the source is confidently excluded.
        assert_eq!(
            group.match_outcome(&info("Movie.2024.1080p.Russian.WEB-DL.mkv"), None),
            MatchOutcome::NoMatch
        );
    }

    // `-Lat` / `LATAM` are common LATAM-Spanish release tags hunch does not
    // recognize; `Castellano` means Castilian Spanish even though hunch maps
    // the token to Catalan. All are separator-bounded.
    #[test]
    fn audio_lang_lat_release_tag_counts_as_spanish() {
        let group = group_eng_spa();
        assert_eq!(
            group.match_outcome(&info("Movie.2024.1080p.WEB-DL-Lat.mkv"), None),
            MatchOutcome::Match
        );
        assert_eq!(
            group.match_outcome(&info("Movie 2024 2160p LATAM WEB-DL.mkv"), None),
            MatchOutcome::Match
        );
        assert_eq!(
            group.match_outcome(&info("Movie.2024.1080p.Latino.WEB-DL.mkv"), None),
            MatchOutcome::Match
        );
        assert_eq!(
            group.match_outcome(&info("Movie.2024.1080p.Castellano.WEB-DL.mkv"), None),
            MatchOutcome::Match
        );
        // Not separator-bounded → no evidence → not a desired language.
        assert_eq!(
            group.match_outcome(&info("Latitude.2024.1080p.WEB-DL.mkv"), None),
            MatchOutcome::NoMatch
        );
        // Subtitle tags do not provide audio-language evidence.
        assert_eq!(
            group.match_outcome(
                &info("Movie.2024.1080p.Japanese.Sub.Lat.WEB-DL.mkv"),
                None
            ),
            MatchOutcome::NoMatch
        );
        assert_eq!(
            group_audio_lang(SetOp::In, &["cat"])
                .match_outcome(&info("Movie.2024.1080p.Castellano.WEB-DL.mkv"), None),
            MatchOutcome::NoMatch
        );
    }

    // Probed track languages are ground truth and override filename tags.
    #[test]
    fn audio_lang_probe_overrides_filename_evidence() {
        let group = group_eng_spa();
        // Filename claims Russian, probe proves an English track.
        assert_eq!(
            group.match_outcome(
                &info("Movie.2024.1080p.Russian.WEB-DL.mkv"),
                Some(&probe_with_langs(&["eng"]))
            ),
            MatchOutcome::Match
        );
        // Filename claims Spanish, probe proves Russian-only audio.
        assert_eq!(
            group.match_outcome(
                &info("Movie.2024.1080p.Latino.WEB-DL.mkv"),
                Some(&probe_with_langs(&["rus"]))
            ),
            MatchOutcome::NoMatch
        );
        // A probe with only special codes is not concrete; filename decides.
        assert_eq!(
            group.match_outcome(
                &info("Movie.2024.1080p.Latino.WEB-DL.mkv"),
                Some(&probe_with_langs(&["und"]))
            ),
            MatchOutcome::Match
        );
    }

    // Title-level original language (BCP 47, as stored on movies/series) is
    // last-resort evidence when probe and filename say nothing.
    #[test]
    fn audio_lang_falls_back_to_original_language() {
        let group = group_eng_spa();
        assert_eq!(
            group.match_outcome_ctx(
                &info("Movie.2024.1080p.WEB-DL.mkv"),
                None,
                Some("en")
            ),
            MatchOutcome::Match
        );
        assert_eq!(
            group.match_outcome_ctx(
                &info("Movie.2024.1080p.WEB-DL.mkv"),
                None,
                Some("ru")
            ),
            MatchOutcome::NoMatch
        );
    }

    #[test]
    fn audio_lang_normalizes_chinese_code_variants() {
        for value in ["chi", "zho", "zh-CN", "Chinese"] {
            assert_eq!(evidence_lang_code(value).as_deref(), Some("chi"));
        }
    }

    // Concrete filename evidence beats the title language: a Russian dub of
    // an English-language movie must not enter an [eng, spa] group.
    #[test]
    fn audio_lang_filename_overrides_original_language() {
        let group = group_eng_spa();
        assert_eq!(
            group.match_outcome_ctx(
                &info("Movie.2024.1080p.Russian.WEB-DL.mkv"),
                None,
                Some("en")
            ),
            MatchOutcome::NoMatch
        );
    }

    // ── Representative quality ranking ──────────────────────────────────

    fn source(filename: &str) -> Media {
        Media {
            stream_info: Some(info(filename)),
            ..Default::default()
        }
    }

    // Candidates rank by the quality ladder; the stable sort keeps input
    // (raw/idx) order for ties.
    #[test]
    fn rank_by_quality_orders_candidates_by_ladder() {
        let mut candidates = vec![
            source("Movie.2024.1080p.HDTV.x264.mkv"),
            source("Movie.2024.1080p.WEB-DL.mkv"),
            source("Movie.2024.1080p.BluRay.REMUX.AVC.mkv"),
            source("Movie.2024.1080p.WEBRip.x264.mkv"),
            source("Movie.2024.1080p.WEB-DL.DDP5.1.mkv"),
        ];
        rank_by_quality(&mut candidates);
        let order: Vec<&str> = candidates
            .iter()
            .filter_map(|s| {
                s.stream_info
                    .as_ref()?
                    .filename
                    .as_deref()
            })
            .collect();
        assert_eq!(
            order,
            vec![
                "Movie.2024.1080p.BluRay.REMUX.AVC.mkv",
                "Movie.2024.1080p.WEB-DL.mkv",
                "Movie.2024.1080p.WEB-DL.DDP5.1.mkv",
                "Movie.2024.1080p.WEBRip.x264.mkv",
                "Movie.2024.1080p.HDTV.x264.mkv",
            ]
        );
    }

    #[test]
    fn rank_by_quality_prefers_resolution_before_source() {
        let mut candidates = vec![
            source("Movie.2024.360p.BluRay.REMUX.mkv"),
            source("Movie.2024.BluRay.REMUX.mkv"),
            source("Movie.2024.480p.WEB-DL.mkv"),
        ];
        rank_by_quality(&mut candidates);
        let order: Vec<&str> = candidates
            .iter()
            .filter_map(|source| {
                source
                    .stream_info
                    .as_ref()?
                    .filename
                    .as_deref()
            })
            .collect();
        assert_eq!(
            order,
            vec![
                "Movie.2024.480p.WEB-DL.mkv",
                "Movie.2024.360p.BluRay.REMUX.mkv",
                "Movie.2024.BluRay.REMUX.mkv",
            ]
        );
    }

    fn group_size(op: NumericOp, value: i64) -> StreamGroup {
        StreamGroup {
            id: Uuid::nil(),
            name: "size".to_string(),
            filter: StreamFilter {
                match_mode: FilterMatchMode::All,
                rules: vec![StreamRule::Size { op, value }],
            },
            priority: 0,
            enabled: true,
            hidden: false,
            created_at: String::new(),
        }
    }

    fn info_with_size(filename: &str, size: Option<i64>) -> StreamInfo {
        StreamInfo {
            descriptor: StreamDescriptor::default(),
            filename: Some(filename.to_string()),
            size,
            ..Default::default()
        }
    }

    #[test]
    fn size_gt_matches_large_release() {
        let group = group_size(NumericOp::Gt, 20_000_000_000);
        assert_eq!(
            group.match_outcome(
                &info_with_size("Movie.2024.1080p.BluRay.mkv", Some(35_000_000_000)),
                None
            ),
            MatchOutcome::Match
        );
    }

    #[test]
    fn size_gt_rejects_small_release() {
        let group = group_size(NumericOp::Gt, 20_000_000_000);
        assert_eq!(
            group.match_outcome(
                &info_with_size("Movie.2024.1080p.WEBRip.mkv", Some(2_000_000_000)),
                None
            ),
            MatchOutcome::NoMatch
        );
    }

    // HTTP/debrid/IPTV sources almost never report a size. Unknown size passes
    // through (PassThrough) so they stay visible via the ungrouped list.
    #[test]
    fn size_unknown_is_passthrough() {
        let group = group_size(NumericOp::Gt, 20_000_000_000);
        assert_eq!(
            group.match_outcome(&info_with_size("Movie.2024.1080p.mkv", None), None),
            MatchOutcome::PassThrough
        );
    }

    #[test]
    fn size_ops_eq_not_eq_lt() {
        let v = 5_000_000_000;
        assert_eq!(
            group_size(NumericOp::Eq, v)
                .match_outcome(&info_with_size("a.mkv", Some(v)), None),
            MatchOutcome::Match
        );
        assert_eq!(
            group_size(NumericOp::Eq, v)
                .match_outcome(&info_with_size("a.mkv", Some(v + 1)), None),
            MatchOutcome::NoMatch
        );
        assert_eq!(
            group_size(NumericOp::NotEq, v)
                .match_outcome(&info_with_size("a.mkv", Some(v + 1)), None),
            MatchOutcome::Match
        );
        assert_eq!(
            group_size(NumericOp::Lt, v)
                .match_outcome(&info_with_size("a.mkv", Some(v - 1)), None),
            MatchOutcome::Match
        );
    }

    #[test]
    fn size_combined_with_resolution_all() {
        let group = StreamGroup {
            id: Uuid::nil(),
            name: "1080p · big".to_string(),
            filter: StreamFilter {
                match_mode: FilterMatchMode::All,
                rules: vec![
                    StreamRule::Resolution {
                        op: SetOp::In,
                        values: vec![StreamResolution::R1080p],
                    },
                    StreamRule::Size {
                        op: NumericOp::Gt,
                        value: 20_000_000_000,
                    },
                ],
            },
            priority: 0,
            enabled: true,
            hidden: false,
            created_at: String::new(),
        };
        assert_eq!(
            group.match_outcome(
                &info_with_size("Movie.2024.1080p.BluRay.mkv", Some(35_000_000_000)),
                None
            ),
            MatchOutcome::Match
        );
        assert_eq!(
            group.match_outcome(
                &info_with_size("Movie.2024.1080p.WEBRip.mkv", Some(1_000_000_000)),
                None
            ),
            MatchOutcome::NoMatch
        );
        assert_eq!(
            group.match_outcome(
                &info_with_size("Movie.2024.720p.BluRay.mkv", Some(35_000_000_000)),
                None
            ),
            MatchOutcome::NoMatch
        );
    }
}
