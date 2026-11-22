//! M1 compositing Tauri commands (ADR 0007): tracks, blend modes, the
//! sequence canvas size and the keyer matte view. Each parses its IPC
//! arguments and forwards to the matching `Engine` method.

use vance_timeline::{BlendMode, ClipId, TrackId, TrackKind};

use crate::engine::EngineHandle;

fn parse_track_id(track_id: &str) -> Result<TrackId, String> {
    track_id
        .parse::<uuid::Uuid>()
        .map(TrackId::from_uuid)
        .map_err(|e| format!("invalid track id: {e}"))
}

fn parse_clip_id(clip_id: &str) -> Result<ClipId, String> {
    clip_id
        .parse::<uuid::Uuid>()
        .map(ClipId::from_uuid)
        .map_err(|e| format!("invalid clip id: {e}"))
}

/// Exact `BlendMode::as_str` names only: IPC input is rejected rather than
/// silently mapped to Normal (that leniency is for loading old bundles).
fn parse_blend_mode(name: &str) -> Result<BlendMode, String> {
    BlendMode::ALL
        .into_iter()
        .find(|m| m.as_str() == name)
        .ok_or_else(|| format!("unknown blend mode: {name}"))
}

/// Add a track; `kind` is "video" or "audio". Returns the new track's id.
#[tauri::command(async)]
pub fn add_track(
    state: tauri::State<'_, EngineHandle>,
    kind: String,
    name: Option<String>,
    position: Option<usize>,
) -> Result<String, String> {
    let kind = match kind.as_str() {
        "video" => TrackKind::Video,
        "audio" => TrackKind::Audio,
        other => return Err(format!("unknown track kind: {other}")),
    };
    state
        .engine()?
        .add_track(kind, name, position)
        .map(|id| id.to_string())
        .map_err(|e| e.to_string())
}

#[tauri::command(async)]
pub fn remove_track(state: tauri::State<'_, EngineHandle>, track_id: String) -> Result<(), String> {
    let id = parse_track_id(&track_id)?;
    state.engine()?.remove_track(id).map_err(|e| e.to_string())
}

/// Rename a track; an empty name reverts it to its positional name.
#[tauri::command(async)]
pub fn rename_track(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    name: String,
) -> Result<(), String> {
    let id = parse_track_id(&track_id)?;
    state
        .engine()?
        .rename_track(id, name)
        .map_err(|e| e.to_string())
}

#[tauri::command(async)]
pub fn move_track(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    to_position: usize,
) -> Result<(), String> {
    let id = parse_track_id(&track_id)?;
    state
        .engine()?
        .move_track(id, to_position)
        .map_err(|e| e.to_string())
}

#[tauri::command(async)]
pub fn set_track_enabled(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    enabled: bool,
) -> Result<(), String> {
    let id = parse_track_id(&track_id)?;
    state
        .engine()?
        .set_track_enabled(id, enabled)
        .map_err(|e| e.to_string())
}

/// Lock or unlock a track (a locked track can't be deleted until unlocked).
#[tauri::command(async)]
pub fn set_track_locked(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    locked: bool,
) -> Result<(), String> {
    let id = parse_track_id(&track_id)?;
    state
        .engine()?
        .set_track_locked(id, locked)
        .map_err(|e| e.to_string())
}

#[tauri::command(async)]
pub fn set_clip_blend_mode(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    blend_mode: String,
) -> Result<(), String> {
    let track = parse_track_id(&track_id)?;
    let clip = parse_clip_id(&clip_id)?;
    let mode = parse_blend_mode(&blend_mode)?;
    state
        .engine()?
        .set_clip_blend_mode(track, clip, mode)
        .map_err(|e| e.to_string())
}

/// Set the sequence canvas size; both `null` clears it to the default.
#[tauri::command(async)]
pub fn set_frame_size(
    state: tauri::State<'_, EngineHandle>,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<(), String> {
    let size = match (width, height) {
        (Some(w), Some(h)) => Some((w, h)),
        (None, None) => None,
        _ => return Err("width and height must both be set or both be null".into()),
    };
    state
        .engine()?
        .set_frame_size(size)
        .map_err(|e| e.to_string())
}

/// Solo a clip's keyer matte in the viewer; `null` clears it.
#[tauri::command(async)]
pub fn set_matte_view(
    state: tauri::State<'_, EngineHandle>,
    clip_id: Option<String>,
) -> Result<(), String> {
    let clip = clip_id.as_deref().map(parse_clip_id).transpose()?;
    state
        .engine()?
        .set_matte_view(clip)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_mode_names_are_strict() {
        for m in BlendMode::ALL {
            assert_eq!(parse_blend_mode(m.as_str()), Ok(m));
        }
        assert!(parse_blend_mode("SoftLight").is_err());
        assert!(parse_blend_mode("bogus").is_err());
    }

    #[test]
    fn ids_must_be_uuids() {
        assert!(parse_track_id("not-a-uuid").is_err());
        let id = TrackId::new();
        assert_eq!(parse_track_id(&id.to_string()), Ok(id));
        let c = ClipId::new();
        assert_eq!(parse_clip_id(&c.to_string()), Ok(c));
    }
}
