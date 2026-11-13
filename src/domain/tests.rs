use super::*;
#[test]
fn validates_limits_without_disabling_lower_bound() {
    let mut s = Settings {
        sr: [4.0, 10.0],
        ..Settings::default()
    };
    s.validate().unwrap();
    assert_eq!(s.sr, [4.0, -1.0]);
    s.sr = [4.0, 3.0];
    assert!(s.validate().is_err());
    s.sr = [0.0, f64::NAN];
    assert!(s.validate().is_err());
    s.sr = [0.0, -1.0];
    s.cooldown = f64::INFINITY;
    assert!(s.validate().is_err());
}
#[test]
fn aliases_preserve_public_setting_keys() {
    let settings: Settings =
        serde_json::from_value(serde_json::json!({"sub_only":true,"pointsOnly":true})).unwrap();
    let value = serde_json::to_value(settings).unwrap();
    assert_eq!(value["sub-only"], true);
    assert_eq!(value["points-only"], true);
    assert!(value.get("sub_only").is_none());
}
