use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const MIN_TIMELINE_SECONDS: f64 = 60.0;
const EPSILON: f64 = 1e-7;

fn default_true() -> bool { true }

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum TrackId {
    #[serde(rename = "v1")]
    V1,
    #[serde(rename = "v2")]
    V2,
    #[serde(rename = "v3")]
    V3,
    #[serde(rename = "v4")]
    V4,
    #[serde(rename = "a1")]
    A1,
    #[serde(rename = "a2")]
    A2,
    #[serde(rename = "a3")]
    A3,
    #[serde(rename = "a4")]
    A4,
    #[serde(rename = "a5")]
    A5,
    #[serde(rename = "a6")]
    A6,
}

impl TrackId {
    pub fn is_video(self) -> bool {
        matches!(self, Self::V1 | Self::V2 | Self::V3 | Self::V4)
    }

    fn lanes(self) -> &'static [TrackId] {
        if self.is_video() {
            &[Self::V1, Self::V2, Self::V3, Self::V4]
        } else {
            &[Self::A1, Self::A2, Self::A3, Self::A4, Self::A5, Self::A6]
        }
    }

    /// Stable lane order shared by timeline serialization, preview and export.
    pub const fn sort_index(self) -> usize {
        match self {
            Self::V1 => 0, Self::V2 => 1, Self::V3 => 2, Self::V4 => 3,
            Self::A1 => 4, Self::A2 => 5, Self::A3 => 6, Self::A4 => 7,
            Self::A5 => 8, Self::A6 => 9,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Clip {
    pub id: String,
    pub track: TrackId,
    pub start: f64,
    #[serde(rename = "dur")]
    pub duration: f64,
    pub label: String,
    #[serde(default, rename = "linkedAudioId")]
    pub linked_clip_id: Option<String>,
    #[serde(default, flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl Clip {
    pub fn end(&self) -> f64 {
        self.start + self.duration
    }

    fn validate(&self) -> Result<(), TimelineError> {
        if self.id.trim().is_empty() || self.label.trim().is_empty() {
            return Err(TimelineError::InvalidClip(
                "clip id and label are required".into(),
            ));
        }
        if !self.start.is_finite() || self.start < 0.0 {
            return Err(TimelineError::InvalidClip(
                "start must be finite and non-negative".into(),
            ));
        }
        if !self.duration.is_finite() || self.duration <= 0.0 {
            return Err(TimelineError::InvalidClip(
                "duration must be finite and positive".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Timeline {
    pub clips: Vec<Clip>,
    pub duration_seconds: f64,
    pub snapping: bool,
    #[serde(default = "default_true")]
    pub magnetic: bool,
}

impl Default for Timeline {
    fn default() -> Self {
        Self {
            clips: Vec::new(),
            duration_seconds: MIN_TIMELINE_SECONDS,
            snapping: true,
            magnetic: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "message", rename_all = "camelCase")]
pub enum TimelineError {
    InvalidClip(String),
    DuplicateId(String),
    ClipNotFound(String),
    InvalidRange,
    NoFreeTrack,
}

/// Keyframe `time` values are absolute timeline timestamps. Keep them attached
/// to their clip whenever a timeline operation changes that clip's start.
fn shift_clip_keyframes(clip: &mut Clip, delta: f64) {
    if !delta.is_finite() || delta.abs() <= EPSILON {
        return;
    }
    let Some(keyframes) = clip
        .extra
        .get_mut("keyframes")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for keyframe in keyframes {
        let Some(time) = keyframe
            .get("time")
            .and_then(serde_json::Value::as_f64)
        else {
            continue;
        };
        if time.is_finite() {
            keyframe["time"] = serde_json::json!(time + delta);
        }
    }
}

impl Timeline {
    pub fn new(clips: Vec<Clip>) -> Result<Self, TimelineError> {
        let mut timeline = Self {
            clips,
            ..Self::default()
        };
        timeline.validate_all()?;
        timeline.refresh_duration();
        Ok(timeline)
    }

    pub fn visible_tracks(&self) -> HashSet<TrackId> {
        let mut tracks = HashSet::from([TrackId::V1]);
        tracks.extend(self.clips.iter().map(|clip| clip.track));
        tracks
    }

    pub fn add_clip(&mut self, mut clip: Clip) -> Result<(), TimelineError> {
        clip.validate()?;
        if self.clips.iter().any(|current| current.id == clip.id) {
            return Err(TimelineError::DuplicateId(clip.id));
        }
        if clip.track.is_video() && self.snapping {
            let previous_start = clip.start;
            clip.start = self.snap_start(&clip.id, clip.start.max(0.0), clip.duration, 0.0, 0.25, None);
            let start_delta = clip.start - previous_start;
            shift_clip_keyframes(&mut clip, start_delta);
        }
        if clip.track.is_video() && self.magnetic {
            let previous_start = clip.start;
            clip.start = self.magnetic_start(&clip.id, clip.start, clip.duration, None);
            let start_delta = clip.start - previous_start;
shift_clip_keyframes(&mut clip, start_delta);
        }
        let requested_track = clip.track;
        clip.track = self.nearest_free_lane(&clip.id, requested_track, clip.start, clip.duration)
            .unwrap_or(requested_track);
        let needs_ripple = clip.track == requested_track
            && self.lane_overlaps(&clip.id, clip.track, clip.start, clip.duration);
        let id = clip.id.clone();
        self.clips.push(clip);
        if needs_ripple && self.magnetic {
            self.ripple_lane_after(&id)?;
        }
        self.refresh_duration();
        Ok(())
    }

    /// Move a clip to the closest available lane of the same media kind.
    /// Snap candidates include other clip boundaries and the playhead.
    pub fn move_clip(
        &mut self,
        id: &str,
        desired_start: f64,
        desired_track: TrackId,
        playhead: f64,
        snap_threshold: f64,
    ) -> Result<Clip, TimelineError> {
        if !desired_start.is_finite()
            || !playhead.is_finite()
            || !snap_threshold.is_finite()
            || snap_threshold < 0.0
        {
            return Err(TimelineError::InvalidRange);
        }
        let original = self.find(id)?.clone();
        if original.track.is_video() != desired_track.is_video() {
            return Err(TimelineError::InvalidRange);
        }
        let group_id = original.extra.get("groupId").and_then(serde_json::Value::as_str).map(str::to_owned);
        let mut members: Vec<_> = self.clips.iter().filter(|clip| {
            group_id.as_deref().is_some_and(|group| clip.extra.get("groupId").and_then(serde_json::Value::as_str) == Some(group))
        }).cloned().collect();
        if members.is_empty() { members.push(original.clone()); }
        let mut start = desired_start.max(0.0);
        // Proximity snapping is optional and threshold-based. Magnetic placement
        // is separate: it always aligns video clips to the nearest video edge,
        // even when proximity snapping is disabled.
        if self.snapping && original.track.is_video() {
            start = self.snap_start(id, start, original.duration, playhead, snap_threshold, group_id.as_deref());
        }
        if original.track.is_video() && self.magnetic {
            let moved_ids: HashSet<String> = members.iter().map(|clip| clip.id.clone()).collect();
            if (start - original.start).abs() > EPSILON || desired_track != original.track {
                let vacated: Vec<_> = members.iter().filter(|clip| clip.track.is_video()).cloned().collect();
                self.close_video_gaps(&vacated, &moved_ids);
            }
            start = self.magnetic_start(id, start, original.duration, group_id.as_deref());
        }
        let lane = self.nearest_free_lane_excluding(id, desired_track, start, original.duration, group_id.as_deref())
            .unwrap_or(desired_track);
        let delta = start - original.start;
        let lanes = original.track.lanes();
        let old_rank = lanes.iter().position(|track| *track == original.track).unwrap_or(0) as i32;
        let new_rank = lanes.iter().position(|track| *track == lane).unwrap_or(old_rank as usize) as i32;
        let requested_track_shift = new_rank - old_rank;
        let same_kind: Vec<_> = members.iter().filter(|clip| clip.track.is_video() == original.track.is_video()).collect();
        let min_rank = same_kind.iter().map(|clip| clip.track.lanes().iter().position(|track| *track == clip.track).unwrap_or(0) as i32).min().unwrap_or(old_rank);
        let max_rank = same_kind.iter().map(|clip| clip.track.lanes().iter().position(|track| *track == clip.track).unwrap_or(0) as i32).max().unwrap_or(old_rank);
        let max_lane = lanes.len().saturating_sub(1) as i32;
        let track_shift = requested_track_shift.clamp(-min_rank, max_lane - max_rank);

        let lane_members: HashSet<_> = same_kind.iter().map(|clip| clip.id.as_str()).collect();
        for mut member in members.iter().cloned() {
            let old_member_start = member.start;
            member.start = (member.start + delta).max(0.0);
            let member_delta = member.start - old_member_start;
            shift_clip_keyframes(&mut member, member_delta);
            if lane_members.contains(member.id.as_str()) {
                let member_rank = member.track.lanes().iter().position(|track| *track == member.track).unwrap_or(0) as i32;
                if let Some(track) = member.track.lanes().get((member_rank + track_shift) as usize) { member.track = *track; }
            }
            self.replace(member);
        }

        let mut moved_ids: HashSet<String> = members.iter().map(|clip| clip.id.clone()).collect();
        for member in &members {
            if let Some(linked_id) = member.linked_clip_id.as_deref() {
                if moved_ids.insert(linked_id.to_owned()) {
                    if let Some(mut linked) = self.clips.iter().find(|clip| clip.id == linked_id).cloned() {
                        let old_linked_start = linked.start;
                        linked.start = (linked.start + delta).max(0.0);
                        let linked_delta = linked.start - old_linked_start;
                        shift_clip_keyframes(&mut linked, linked_delta);
                        self.replace(linked);
                    }
                }
            }
        }
        let mut ordered_clips: Vec<_> = moved_ids.into_iter()
            .filter_map(|moved_id| self.clips.iter().find(|clip| clip.id == moved_id).cloned())
            .collect();
        ordered_clips.sort_by(|a, b| {
            a.track.sort_index().cmp(&b.track.sort_index())
                .then_with(|| a.start.total_cmp(&b.start))
        });
        if self.magnetic {
            for moved in ordered_clips {
                if moved.track.is_video() { self.ripple_lane_after(&moved.id)?; }
            }
        }
        self.refresh_duration();
        self.find(id).cloned()
    }

    pub fn trim_clip(&mut self, id: &str, start: f64, end: f64, source_duration: Option<f64>) -> Result<Clip, TimelineError> {
        if !start.is_finite() || !end.is_finite() || start < 0.0 || end - start <= EPSILON {
            return Err(TimelineError::InvalidRange);
        }
        let mut clip = self.find(id)?.clone();
        if let Some(duration) = source_duration.filter(|duration| duration.is_finite() && *duration > 0.0) {
            clip.extra.insert("sourceDuration".into(), serde_json::json!(duration));
        }
        let source_offset = clip.extra.get("sourceOffset")
            .and_then(serde_json::Value::as_f64).unwrap_or(0.0).max(0.0);
        let source_duration = clip.extra.get("sourceDuration")
            .and_then(serde_json::Value::as_f64).filter(|duration| duration.is_finite() && *duration > 0.0);
        /* A left trim changes source offset. The source start and end stay
           bounded so dragging a handle back restores already-trimmed media. */
        let next_start = start.max((clip.start - source_offset).max(0.0));
        let next_offset = (source_offset + next_start - clip.start).max(0.0);
        let next_end = source_duration
            .map(|duration| end.min(next_start + (duration - next_offset).max(0.0)))
            .unwrap_or_else(|| end.min(clip.end()));
        if next_end - next_start <= EPSILON { return Err(TimelineError::InvalidRange); }

        let linked_id = clip.linked_clip_id.clone();
        let linked_track = linked_id.as_deref()
            .and_then(|linked| self.clips.iter().find(|candidate| candidate.id == linked))
            .map(|linked| linked.track);
        let mut changing_ids = vec![id];
        if let Some(linked) = linked_id.as_deref() { changing_ids.push(linked); }
        if self.clips.iter().any(|other| {
            !changing_ids.contains(&other.id.as_str())
                && (other.track == clip.track || Some(other.track) == linked_track)
                && next_start < other.end() - EPSILON
                && next_end > other.start + EPSILON
        }) {
            return Err(TimelineError::InvalidRange);
        }
        let previous_start = clip.start;
        clip.start = next_start;
        let start_delta = clip.start - previous_start;
        shift_clip_keyframes(&mut clip, start_delta);
        clip.duration = next_end - next_start;
        clip.extra.insert("sourceOffset".into(), serde_json::json!(next_offset));
        self.replace(clip.clone());
        if let Some(linked_id) = linked_id {
            if let Some(mut linked) = self.clips.iter().find(|candidate| candidate.id == linked_id).cloned() {
                let previous_linked_start = linked.start;
                linked.start = next_start;
                let linked_delta = linked.start - previous_linked_start;
                shift_clip_keyframes(&mut linked, linked_delta);
                linked.duration = next_end - next_start;
                linked.extra.insert("sourceOffset".into(), serde_json::json!(next_offset));
                if let Some(duration) = source_duration { linked.extra.insert("sourceDuration".into(), serde_json::json!(duration)); }
                self.replace(linked);
            }
        }
        self.refresh_duration();
        self.find(id).cloned()
    }

    pub fn split_clip(
        &mut self,
        id: &str,
        at: f64,
        right_id: String,
    ) -> Result<(Clip, Clip), TimelineError> {
        if !at.is_finite()
            || right_id.trim().is_empty()
            || self.clips.iter().any(|c| c.id == right_id)
        {
            return Err(TimelineError::InvalidRange);
        }
        let mut left = self.find(id)?.clone();
        if at <= left.start + EPSILON || at >= left.end() - EPSILON {
            return Err(TimelineError::InvalidRange);
        }
        let split_offset = at - left.start;
        let original_source_offset = left.extra.get("sourceOffset")
            .and_then(serde_json::Value::as_f64).unwrap_or(0.0);
        let mut right_extra = left.extra.clone();
        right_extra.insert("sourceOffset".into(), serde_json::json!(original_source_offset + split_offset));
        let right = Clip {
            id: right_id,
            track: left.track,
            start: at,
            duration: left.end() - at,
            label: left.label.clone(),
            linked_clip_id: None,
            extra: right_extra,
        };
        left.duration = at - left.start;
        /* The original end transition follows the tail segment after a cut. */
        if let Some(transition) = left.extra.get_mut("props").and_then(serde_json::Value::as_object_mut)
            .and_then(|props| props.get_mut("transitionOut")).and_then(serde_json::Value::as_object_mut) {
            transition.insert("type".into(), serde_json::json!("none"));
        }
        self.replace(left.clone());
        self.clips.push(right.clone());
        self.refresh_duration();
        Ok((left, right))
    }

    pub fn delete_clip(&mut self, id: &str) -> Result<Vec<Clip>, TimelineError> {
        let clip = self.find(id)?.clone();
        let mut deleted = vec![clip.clone()];
        if let Some(linked) = clip.linked_clip_id.as_deref() {
            if let Some(other) = self.clips.iter().find(|c| c.id == linked) {
                deleted.push(other.clone());
            }
        }
        let deleted_ids: HashSet<String> = deleted.iter().map(|c| c.id.clone()).collect();
        if clip.track.is_video() && self.magnetic {
            self.close_video_gaps(&[clip.clone()], &deleted_ids);
        }
        self.clips.retain(|c| !deleted_ids.contains(&c.id));
        self.refresh_duration();
        Ok(deleted)
    }

    pub fn group_clips(&mut self, ids: &[String], group_id: &str) -> Result<(), TimelineError> {
        let unique: HashSet<_> = ids.iter().map(String::as_str).collect();
        if unique.len() < 2 || group_id.trim().is_empty() { return Err(TimelineError::InvalidRange); }
        for id in &unique { self.find(id)?; }
        for id in unique {
            let clip = self.clips.iter_mut().find(|clip| clip.id == id).ok_or_else(|| TimelineError::ClipNotFound(id.into()))?;
            clip.extra.insert("groupId".into(), serde_json::json!(group_id));
        }
        Ok(())
    }

    pub fn ungroup_clip(&mut self, id: &str) -> Result<(), TimelineError> {
        let group_id = self.find(id)?.extra.get("groupId").and_then(serde_json::Value::as_str)
            .ok_or(TimelineError::InvalidRange)?.to_owned();
        for clip in &mut self.clips {
            if clip.extra.get("groupId").and_then(serde_json::Value::as_str) == Some(group_id.as_str()) {
                clip.extra.remove("groupId");
            }
        }
        Ok(())
    }

    pub fn separate_audio(&mut self, id: &str, audio_id: String) -> Result<Clip, TimelineError> {
        if audio_id.trim().is_empty() || self.clips.iter().any(|clip| clip.id == audio_id) {
            return Err(TimelineError::InvalidRange);
        }
        let mut video = self.find(id)?.clone();
        if !video.track.is_video()
            || video.extra.get("hasAudio").and_then(serde_json::Value::as_bool) != Some(true)
            || video.extra.get("kind").and_then(serde_json::Value::as_str) == Some("audio")
        { return Err(TimelineError::InvalidRange); }
        let mut audio = video.clone();
        audio.id = audio_id.clone();
        audio.track = self.nearest_free_lane(&audio_id, TrackId::A1, audio.start, audio.duration)?;
        audio.label = format!("{} Audio", video.label);
        audio.linked_clip_id = Some(video.id.clone());
        audio.extra.insert("kind".into(), serde_json::json!("audio"));
        audio.extra.insert("hasAudio".into(), serde_json::json!(true));
        video.linked_clip_id = Some(audio_id.clone());
        video.extra.insert("hasAudio".into(), serde_json::json!(false));
        self.replace(video);
        self.add_clip(audio)?;
        Ok(self.find(&audio_id)?.clone())
    }

    pub fn unlink_clip(&mut self, id: &str) -> Result<(), TimelineError> {
        let mut clip = self.find(id)?.clone();
        let linked_id = clip.linked_clip_id.take().ok_or(TimelineError::InvalidRange)?;
        self.replace(clip);
        if let Some(mut linked) = self.clips.iter().find(|current| current.id == linked_id).cloned() {
            linked.linked_clip_id = None;
            self.replace(linked);
        }
        Ok(())
    }

    fn snap_start(
        &self,
        id: &str,
        start: f64,
        duration: f64,
        playhead: f64,
        threshold: f64,
        exclude_group: Option<&str>,
    ) -> f64 {
        let mut best: Option<(f64, f64)> = None;
        for candidate in std::iter::once(playhead).chain(
            self.clips
                .iter()
                .filter(|c| c.id != id && c.track.is_video()
                    && exclude_group.map_or(true, |group| c.extra.get("groupId").and_then(serde_json::Value::as_str) != Some(group)))
                .flat_map(|c| [c.start, c.end()]),
        ) {
            for (edge, offset) in [(start, 0.0), (start + duration, duration)] {
                let distance = (candidate - edge).abs();
                if distance <= threshold && best.map_or(true, |(d, _)| distance < d) {
                    best = Some((distance, candidate - offset));
                }
            }
        }
        best.map_or(start, |(_, value)| value.max(0.0))
    }

    /// Place video clips against the nearest edge in any video lane. Unlike
    /// proximity snapping this has no distance threshold and ignores audio.
    /// Candidate edges align this clip either immediately after an existing clip
    /// or immediately before one, making a shared magnetic video storyline.
    fn magnetic_start(&self, id: &str, start: f64, duration: f64, exclude_group: Option<&str>) -> f64 {
        let mut best_start = start.max(0.0);
        let mut best_distance = f64::INFINITY;
        for clip in self.clips.iter().filter(|clip| {
            clip.track.is_video()
                && clip.id != id
                && exclude_group.map_or(true, |group| {
                    clip.extra.get("groupId").and_then(serde_json::Value::as_str) != Some(group)
                })
        }) {
            let candidates = [clip.end(), clip.start - duration];
            for candidate in candidates.into_iter().filter(|value| value.is_finite() && *value >= 0.0) {
                let distance = (candidate - start).abs();
                if distance < best_distance - EPSILON
                    || ((distance - best_distance).abs() <= EPSILON && candidate < best_start)
                {
                    best_distance = distance;
                    best_start = candidate;
                }
            }
        }
        best_start
    }

    /// Collapse removed video time ranges across all video lanes. Clips that
    /// begin after a removed range slide left by its duration, while each lane
    /// retains its own non-overlapping order. Audio-only lanes do not participate;
    /// an audio clip explicitly linked to a moved video clip follows that clip.
    fn close_video_gaps(&mut self, removed: &[Clip], excluded_ids: &HashSet<String>) {
        let mut intervals: Vec<(f64, f64)> = removed.iter()
            .filter(|clip| clip.track.is_video() && clip.duration > EPSILON)
            .map(|clip| (clip.start, clip.end()))
            .collect();
        if intervals.is_empty() { return; }
        intervals.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f64, f64)> = Vec::with_capacity(intervals.len());
        for (start, end) in intervals {
            if let Some(last) = merged.last_mut() {
                if start <= last.1 + EPSILON {
                    last.1 = last.1.max(end);
                    continue;
                }
            }
            merged.push((start, end));
        }

        let mut followers: Vec<Clip> = self.clips.iter()
            .filter(|clip| clip.track.is_video() && !excluded_ids.contains(&clip.id))
            .cloned().collect();
        followers.sort_by(|a, b| {
            a.track.sort_index().cmp(&b.track.sort_index())
                .then_with(|| a.start.total_cmp(&b.start))
                .then_with(|| a.id.cmp(&b.id))
        });

        for mut clip in followers {
            let original_start = clip.start;
            let shift: f64 = merged.iter()
                .filter(|(_, end)| *end <= original_start + EPSILON)
                .map(|(start, end)| end - start)
                .sum();
            if shift <= EPSILON { continue; }

            let desired = (original_start - shift).max(0.0);
            let previous_end = self.clips.iter()
                .filter(|other| other.track == clip.track
                    && other.id != clip.id
                    && !excluded_ids.contains(&other.id)
                    && other.start < original_start - EPSILON)
                .map(Clip::end)
                .fold(0.0, f64::max);
            let next_start = desired.max(previous_end.min(original_start)).min(original_start);
            let delta = next_start - original_start;
            if delta.abs() <= EPSILON { continue; }

            clip.start = next_start;
            shift_clip_keyframes(&mut clip, delta);
            let linked_id = clip.linked_clip_id.clone();
            self.replace(clip);

            if let Some(linked_id) = linked_id {
                if let Some(mut linked) = self.clips.iter().find(|item| item.id == linked_id).cloned() {
                    linked.start = (linked.start + delta).max(0.0);
                    shift_clip_keyframes(&mut linked, delta);
                    self.replace(linked);
                }
            }
        }
    }

    fn lane_overlaps(&self, id: &str, track: TrackId, start: f64, duration: f64) -> bool {
        self.clips.iter().any(|c| {
            c.id != id && c.track == track
                && start < c.end() - EPSILON
                && start + duration > c.start + EPSILON
        })
    }

    /// Keep the dropped/moved clip at its chosen time and push any clips it
    /// collides with later on the same lane, preserving their order.
    fn ripple_lane_after(&mut self, anchor_id: &str) -> Result<(), TimelineError> {
        let anchor = self.find(anchor_id)?.clone();
        let group_id = anchor.extra.get("groupId").and_then(serde_json::Value::as_str);
        let mut followers: Vec<_> = self.clips.iter()
            .filter(|c| c.id != anchor_id && c.track == anchor.track && c.end() > anchor.start + EPSILON
                && (group_id.is_none() || c.extra.get("groupId").and_then(serde_json::Value::as_str) != group_id))
            .cloned().collect();
        followers.sort_by(|a, b| a.start.total_cmp(&b.start).then_with(|| a.id.cmp(&b.id)));
        let mut cursor = anchor.end();
        for mut clip in followers {
            let previous_start = clip.start;
            if clip.start < cursor - EPSILON { clip.start = cursor; }
            let delta = clip.start - previous_start;
            shift_clip_keyframes(&mut clip, delta);
            if delta.abs() > EPSILON {
                if let Some(linked_id) = clip.linked_clip_id.as_deref() {
                    if let Some(mut linked) = self.clips.iter().find(|item| item.id == linked_id).cloned() {
                        let previous_linked_start = linked.start;
                        linked.start = (linked.start + delta).max(0.0);
                        let linked_delta = linked.start - previous_linked_start;
                        shift_clip_keyframes(&mut linked, linked_delta);
                        self.replace(linked);
                    }
                }
            }
            cursor = clip.end();
            self.replace(clip);
        }
        Ok(())
    }

    fn nearest_free_lane(
        &self,
        id: &str,
        desired: TrackId,
        start: f64,
        duration: f64,
    ) -> Result<TrackId, TimelineError> {
        self.nearest_free_lane_excluding(id, desired, start, duration, None)
    }

    fn nearest_free_lane_excluding(
        &self,
        id: &str,
        desired: TrackId,
        start: f64,
        duration: f64,
        exclude_group: Option<&str>,
    ) -> Result<TrackId, TimelineError> {
        let lanes = desired.lanes();
        let desired_index = lanes.iter().position(|lane| *lane == desired).unwrap_or(0);
        let mut candidates: Vec<_> = lanes.iter().copied().enumerate().collect();
        candidates.sort_by_key(|(index, _)| (index.abs_diff(desired_index), *index));
        candidates
            .into_iter()
            .map(|(_, lane)| lane)
            .find(|lane| {
                !self.clips.iter().any(|c| {
                    c.id != id
                        && exclude_group.map_or(true, |group| c.extra.get("groupId").and_then(serde_json::Value::as_str) != Some(group))
                        && c.track == *lane
                        && start < c.end() - EPSILON
                        && start + duration > c.start + EPSILON
                })
            })
            .ok_or(TimelineError::NoFreeTrack)
    }

    fn find(&self, id: &str) -> Result<&Clip, TimelineError> {
        self.clips
            .iter()
            .find(|clip| clip.id == id)
            .ok_or_else(|| TimelineError::ClipNotFound(id.into()))
    }

    fn replace(&mut self, clip: Clip) {
        if let Some(index) = self.clips.iter().position(|current| current.id == clip.id) {
            self.clips[index] = clip;
        }
    }

    fn validate_all(&self) -> Result<(), TimelineError> {
        let mut ids = HashSet::new();
        for clip in &self.clips {
            clip.validate()?;
            if !ids.insert(&clip.id) {
                return Err(TimelineError::DuplicateId(clip.id.clone()));
            }
        }
        Ok(())
    }

    fn refresh_duration(&mut self) {
        self.duration_seconds = self
            .clips
            .iter()
            .map(Clip::end)
            .fold(MIN_TIMELINE_SECONDS, f64::max);
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn video_clip(id: &str, start: f64, duration: f64) -> Clip {
        Clip {
            id: id.to_owned(), track: TrackId::V1, start, duration,
            label: id.to_owned(), linked_clip_id: None, extra: HashMap::new(),
        }
    }


    #[test]
    fn timeline_starts_with_only_the_primary_video_track_visible() {
        let timeline = Timeline::default();
        assert_eq!(timeline.visible_tracks(), HashSet::from([TrackId::V1]));
    }

    #[test]
    fn all_six_audio_track_ids_round_trip_through_project_json() {
        for (name, track) in [
            ("a1", TrackId::A1), ("a2", TrackId::A2), ("a3", TrackId::A3),
            ("a4", TrackId::A4), ("a5", TrackId::A5), ("a6", TrackId::A6),
        ] {
            assert_eq!(serde_json::from_str::<TrackId>(&format!("\"{name}\"" )).unwrap(), track);
            assert_eq!(serde_json::to_string(&track).unwrap(), format!("\"{name}\""));
        }
    }

    #[test]
    fn separated_audio_uses_the_next_free_audio_layer_when_a1_is_occupied() {
        let mut video = video_clip("video", 1.0, 3.0);
        video.extra.insert("hasAudio".into(), serde_json::json!(true));
        let occupying_audio = Clip {
            id: "music".into(), track: TrackId::A1, start: 1.0, duration: 3.0,
            label: "Music".into(), linked_clip_id: None,
            extra: HashMap::from([("kind".into(), serde_json::json!("audio"))]),
        };
        let mut timeline = Timeline::default();
        timeline.clips.extend([video, occupying_audio]);
        timeline.snapping = false;

        let separated = timeline.separate_audio("video", "video-audio".into()).unwrap();
        assert_eq!(separated.track, TrackId::A2);
        assert_eq!(separated.linked_clip_id.as_deref(), Some("video"));
    }

    #[test]
    fn move_clip_is_free_when_proximity_snapping_is_disabled() {
        let mut timeline = Timeline::default();
        timeline.clips.push(video_clip("one", 0.0, 2.0));
        timeline.snapping = false;
        let moved = timeline.move_clip("one", 5.37, TrackId::V1, 0.0, 0.25).unwrap();
        assert!((moved.start - 5.37).abs() < 1e-9);
    }

    #[test]
    fn moving_clip_keeps_absolute_keyframes_attached_to_the_clip() {
        let mut clip = video_clip("animated", 2.0, 4.0);
        clip.extra.insert("keyframes".into(), serde_json::json!([
            {"time": 2.25, "values": {"x": 0.0}},
            {"time": 4.0, "values": {"x": 100.0}}
        ]));
        let mut timeline = Timeline::default();
        timeline.clips.push(clip);
        timeline.snapping = false;

        timeline.move_clip("animated", 7.0, TrackId::V1, 0.0, 0.0).unwrap();
        let moved = timeline.clips.iter().find(|clip| clip.id == "animated").unwrap();
        let frames = moved.extra["keyframes"].as_array().unwrap();
        assert!((moved.start - 7.0).abs() < 1e-9);
        assert!((frames[0]["time"].as_f64().unwrap() - 7.25).abs() < 1e-9);
        assert!((frames[1]["time"].as_f64().unwrap() - 9.0).abs() < 1e-9);
    }

    #[test]
    fn moving_video_shifts_linked_audio_keyframes_by_the_same_amount() {
        let mut video = video_clip("video", 2.0, 4.0);
        video.linked_clip_id = Some("audio".into());
        let mut audio = Clip {
            id: "audio".into(), track: TrackId::A1, start: 2.0, duration: 4.0,
            label: "video Audio".into(), linked_clip_id: Some("video".into()), extra: HashMap::new(),
        };
        audio.extra.insert("keyframes".into(), serde_json::json!([
            {"time": 2.5, "values": {"volume": 0.0}},
            {"time": 5.5, "values": {"volume": 100.0}}
        ]));
        let mut timeline = Timeline::default();
        timeline.clips.extend([video, audio]);
        timeline.snapping = false;

        timeline.move_clip("video", 7.0, TrackId::V1, 0.0, 0.0).unwrap();
        let video = timeline.clips.iter().find(|clip| clip.id == "video").unwrap();
        let audio = timeline.clips.iter().find(|clip| clip.id == "audio").unwrap();
        assert!((video.start - 7.0).abs() < 1e-9);
        assert!((audio.start - 7.0).abs() < 1e-9);
        assert!((audio.extra["keyframes"][0]["time"].as_f64().unwrap() - 7.5).abs() < 1e-9);
        assert!((audio.extra["keyframes"][1]["time"].as_f64().unwrap() - 10.5).abs() < 1e-9);
    }

    #[test]
    fn magnetic_ripple_is_independent_of_proximity_snapping_and_crosses_video_lanes() {
        let mut timeline = Timeline::default();
        timeline.clips.push(video_clip("anchor", 0.0, 2.0));
        timeline.clips.push(video_clip("moving", 2.0, 2.0));
        let mut upper = video_clip("upper-follower", 6.0, 2.0);
        upper.track = TrackId::V2;
        timeline.clips.push(upper);
        let mut audio = video_clip("audio-follower", 6.0, 2.0);
        audio.track = TrackId::A1;
        timeline.clips.push(audio);
        timeline.snapping = false;
        timeline.magnetic = true;

        timeline.move_clip("moving", 10.0, TrackId::V1, 0.0, 0.25).unwrap();
        let moved = timeline.clips.iter().find(|clip| clip.id == "moving").unwrap();
        let upper = timeline.clips.iter().find(|clip| clip.id == "upper-follower").unwrap();
        let audio = timeline.clips.iter().find(|clip| clip.id == "audio-follower").unwrap();
        assert!((moved.start - 6.0).abs() < 1e-9, "a far video drop should magnetically align to the nearest video edge");
        assert!((upper.start - 4.0).abs() < 1e-9, "video followers on another lane should close the vacated gap");
        assert!((audio.start - 6.0).abs() < 1e-9, "audio lanes must not participate in magnetic ripple");
    }

    #[test]
    fn deleting_video_clip_closes_gap_across_video_lanes_but_not_audio() {
        let mut timeline = Timeline::default();
        timeline.clips.push(video_clip("before", 0.0, 2.0));
        timeline.clips.push(video_clip("remove", 2.0, 3.0));
        let mut upper = video_clip("upper", 7.0, 2.0);
        upper.track = TrackId::V3;
        timeline.clips.push(upper);
        let mut audio = video_clip("music", 7.0, 2.0);
        audio.track = TrackId::A2;
        timeline.clips.push(audio);
        timeline.delete_clip("remove").unwrap();
        assert!((timeline.clips.iter().find(|clip| clip.id == "upper").unwrap().start - 4.0).abs() < 1e-9);
        assert!((timeline.clips.iter().find(|clip| clip.id == "music").unwrap().start - 7.0).abs() < 1e-9);
    }

    #[test]
    fn legacy_timeline_json_defaults_magnetic_mode_on() {
        let value = serde_json::json!({"clips": [], "durationSeconds": 60.0, "snapping": false});
        let timeline: Timeline = serde_json::from_value(value).unwrap();
        assert!(!timeline.snapping);
        assert!(timeline.magnetic);
    }

    #[test]
    fn magnetic_placement_is_independent_of_proximity_snapping_when_adding_video() {
        let mut timeline = Timeline::default();
        timeline.clips.push(video_clip("one", 0.0, 2.0));
        timeline.snapping = false;
        let mut added_clip = video_clip("two", 5.37, 1.0);
        added_clip.extra.insert("keyframes".into(), serde_json::json!([
            {"time": 5.5, "values": {"x": 25.0}}
        ]));
        timeline.add_clip(added_clip).unwrap();
        let added = timeline.clips.iter().find(|clip| clip.id == "two").unwrap();
        assert!((added.start - 2.0).abs() < 1e-9, "magnetic placement aligns to the nearest video edge even with snapping off");
        assert!((added.extra["keyframes"][0]["time"].as_f64().unwrap() - 2.13).abs() < 1e-9,
            "absolute keyframe timestamps must follow the magnetic placement adjustment");
    }

    #[test]
    fn magnetic_placement_pulls_far_video_drops_to_nearest_video_lane_edge_but_ignores_audio() {
        let mut timeline = Timeline::default();
        timeline.clips.push(video_clip("v1", 0.0, 3.0));
        let mut upper = video_clip("v2", 6.0, 2.0);
        upper.track = TrackId::V2;
        timeline.clips.push(upper);
        let mut audio = video_clip("music", 30.0, 4.0);
        audio.track = TrackId::A1;
        timeline.clips.push(audio);
        timeline.snapping = false;

        timeline.add_clip(video_clip("new", 40.0, 1.0)).unwrap();
        let added = timeline.clips.iter().find(|clip| clip.id == "new").unwrap();
        assert!((added.start - 8.0).abs() < 1e-9, "far-away video drops should align to the nearest video edge, not audio");
    }

    #[test]
    fn moving_with_magnet_off_does_not_ripple_following_clips() {
        let mut timeline = Timeline::default();
        timeline.clips.push(video_clip("one", 0.0, 2.0));
        timeline.clips.push(video_clip("two", 2.0, 3.0));
        timeline.snapping = false;
        timeline.magnetic = false;
        timeline.move_clip("one", 6.25, TrackId::V1, 0.0, 0.25).unwrap();
        let moved = timeline.clips.iter().find(|clip| clip.id == "one").unwrap();
        let follower = timeline.clips.iter().find(|clip| clip.id == "two").unwrap();
        assert!((moved.start - 6.25).abs() < 1e-9, "magnetic-off moves must preserve the requested drop position");
        assert!((follower.start - 2.0).abs() < 1e-9, "magnetic-off moves must preserve the lane gap");
    }

    #[test]
    fn moving_with_magnet_on_closes_the_vacated_lane_gap_and_moves_follower_keyframes() {
        let mut timeline = Timeline::default();
        timeline.clips.push(video_clip("one", 0.0, 2.0));
        let mut follower = video_clip("two", 2.0, 3.0);
        follower.extra.insert("keyframes".into(), serde_json::json!([
            {"time": 2.25, "values": {"x": 0.0}},
            {"time": 4.0, "values": {"x": 100.0}}
        ]));
        timeline.clips.push(follower);
        timeline.snapping = true;
        timeline.move_clip("one", 6.25, TrackId::V1, 0.0, 0.0).unwrap();
        let follower = timeline.clips.iter().find(|clip| clip.id == "two").unwrap();
        assert!((follower.start - 0.0).abs() < 1e-9, "magnetic moves should close the vacated gap");
        let frames = follower.extra["keyframes"].as_array().unwrap();
        assert!((frames[0]["time"].as_f64().unwrap() - 0.25).abs() < 1e-9);
        assert!((frames[1]["time"].as_f64().unwrap() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn ripple_pushes_follower_keyframes_with_their_clip() {
        let mut timeline = Timeline::default();
        timeline.clips.push(video_clip("anchor", 0.0, 2.0));
        let mut follower = video_clip("follower", 1.0, 2.0);
        follower.extra.insert("keyframes".into(), serde_json::json!([
            {"time": 1.25, "values": {"x": 0.0}}
        ]));
        timeline.clips.push(follower);
        timeline.ripple_lane_after("anchor").unwrap();
        let follower = timeline.clips.iter().find(|clip| clip.id == "follower").unwrap();
        assert!((follower.start - 2.0).abs() < 1e-9);
        assert!((follower.extra["keyframes"][0]["time"].as_f64().unwrap() - 2.25).abs() < 1e-9);
    }

    #[test]
    fn left_trim_moves_keyframes_with_the_clip_start() {
        let mut clip = video_clip("trimmed", 2.0, 4.0);
        clip.extra.insert("keyframes".into(), serde_json::json!([
            {"time": 2.5, "values": {"x": 0.0}},
            {"time": 5.0, "values": {"x": 100.0}}
        ]));
        let mut timeline = Timeline::default();
        timeline.clips.push(clip);
        timeline.snapping = false;
        timeline.trim_clip("trimmed", 3.0, 6.0, None).unwrap();
        let trimmed = timeline.clips.iter().find(|clip| clip.id == "trimmed").unwrap();
        assert!((trimmed.start - 3.0).abs() < 1e-9);
        let frames = trimmed.extra["keyframes"].as_array().unwrap();
        assert!((frames[0]["time"].as_f64().unwrap() - 3.5).abs() < 1e-9);
        assert!((frames[1]["time"].as_f64().unwrap() - 6.0).abs() < 1e-9);
    }
}
