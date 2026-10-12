use wwrpc::db::PlayerInfo;
use wwrpc::presence::{db_activity, static_activity};

#[test]
fn static_activity_has_expected_shape() {
    let value = static_activity(1_700_000_000_000);
    assert_eq!(value["details"], "Exploring SOL-III");
    assert_eq!(value["assets"]["large_image"], "logo");
    assert_eq!(value["assets"]["large_text"], "Wuthering Waves");
    assert_eq!(value["timestamps"]["start"], 1_700_000_000_000_i64);
    assert!(value.get("state").is_none());
}

#[test]
fn db_activity_shows_union_level_and_region() {
    let info = PlayerInfo {
        uid: "111111111".to_string(),
        union_level: "60".to_string(),
        region: "Europe".to_string(),
        quests_done: Some(123),
        game_version: Some("3.6.13".to_string()),
        display_name: None,
    };
    let value = db_activity(1_700_000_000_000, &info, None, None);
    assert_eq!(value["details"], "Union Level 60");
    assert_eq!(value["state"], "Lv. 60 | Region: Europe");
    assert_eq!(value["timestamps"]["start"], 1_700_000_000_000_i64);
    assert_eq!(value["assets"]["small_image"], "logo");
    assert_eq!(value["assets"]["small_text"], "123 quests • v3.6.13");
}
