//! Full project bundle round-trip with the NEWER fields: a Fusion node-graph
//! comp, a MoGraph title, timeline clips, review comments, and captions must
//! all survive save → reload. Guards against a serde/bundle regression in any
//! of the recently-added Sequence / MediaItem fields (each is
//! `#[serde(default)]`, so a missing/renamed field would silently drop data
//! rather than error).

use vance_app_lib::engine::Engine;

fn video_track(engine: &Engine) -> vance_timeline::TrackId {
    let contents = engine.current_sequence_contents().expect("contents");
    let v1 = contents.tracks.iter().find(|t| t.kind == "video").expect("V1");
    vance_timeline::TrackId::from_uuid(v1.id.parse().unwrap())
}

#[test]
fn project_bundle_round_trips_new_fields() {
    let dir = std::env::temp_dir().join(format!("vance-rt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bundle = dir.join("proj.vance");

    // --- author a project with diverse content ---
    let (fusion_id, mograph_id) = {
        let mut e = Engine::default();
        e.new_project("rt".into()).expect("new project");

        let fusion = e
            .create_fusion_media(Some("Comp".into()), 640, 360, 3.0)
            .expect("fusion media");
        let mograph = {
            let cat = e.mograph_catalog();
            let (key, _) = cat.first().cloned().expect("a mograph template");
            e.create_mograph_media(key, Some("Title".into())).expect("mograph media")
        };
        let fusion_id = vance_timeline::MediaItemId::from_uuid(fusion.id.parse().unwrap());
        let mograph_id = vance_timeline::MediaItemId::from_uuid(mograph.id.parse().unwrap());

        let track = video_track(&e);
        e.insert_clip_at(track, fusion_id, 0.0).expect("insert fusion");
        e.insert_clip_at(track, mograph_id, 3.0).expect("insert mograph");

        // Review comments (one resolved, one with a reply) + a caption.
        let c1 = e.add_review_comment(1500, "sam".into(), "tighten this".into()).expect("comment");
        e.reply_review_comment(
            vance_timeline::id::CommentId::from_uuid(c1.parse().unwrap()),
            "lee".into(),
            "agreed".into(),
        )
        .expect("reply");
        let c2 = e.add_review_comment(2500, "sam".into(), "done".into()).expect("comment2");
        e.resolve_review_comment(
            vance_timeline::id::CommentId::from_uuid(c2.parse().unwrap()),
            true,
        )
        .expect("resolve");
        e.add_caption(1000, 3000, "hello world".into()).expect("caption");

        e.save_project(Some(bundle.to_string_lossy().to_string())).expect("save");
        (fusion_id, mograph_id)
    };

    // --- reload in a fresh engine and verify everything survived ---
    let mut e = Engine::default();
    e.load_project(&bundle.to_string_lossy()).expect("load project");

    // Media library: both synthetic items are back, with their kinds.
    let media = e.media_list();
    assert!(media.iter().any(|m| m.id == fusion_id.to_string() && m.kind == "fusion"), "fusion media");
    assert!(media.iter().any(|m| m.id == mograph_id.to_string() && m.kind == "mograph"), "mograph media");

    // The Fusion node graph survived intact (output node + nodes present).
    let comp = e.fusion_comp(fusion_id).expect("fusion comp back");
    assert!(!comp.nodes.is_empty() && !comp.output.is_empty(), "fusion graph intact");
    assert_eq!(comp.width, 640, "fusion canvas size preserved");

    // The MoGraph instance survived.
    assert!(e.mograph_instance(mograph_id).is_some(), "mograph instance back");

    // Two clips on V1.
    let contents = e.current_sequence_contents().expect("contents");
    let v1 = contents.tracks.iter().find(|t| t.kind == "video").expect("V1");
    assert_eq!(v1.clips.len(), 2, "both clips survived");

    // Review comments: 2 back, sorted, with the reply + resolved state.
    let comments = e.review_comments();
    assert_eq!(comments.len(), 2, "both comments survived");
    assert_eq!(comments[0].timeline_ms, 1500, "sorted by position");
    assert_eq!(comments[0].replies.len(), 1, "reply survived");
    assert_eq!(comments[0].replies[0].author, "lee");
    assert!(comments[1].resolved, "resolved state survived");

    // Captions survived.
    assert_eq!(e.captions().len(), 1, "caption survived");
    assert_eq!(e.captions()[0].text, "hello world");

    let _ = std::fs::remove_dir_all(&dir);
}
