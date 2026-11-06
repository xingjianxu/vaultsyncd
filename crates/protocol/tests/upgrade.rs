//! S091固定开发协议fixture；不冒充前一正式客户端。
use vaultsyncd_protocol::{
    generated::Capabilities,
    negotiation::{self, Error},
};
const OLD: &str = include_str!("../../../tests/contract/fixtures/s091/remote-v1.0.json");
#[test]
fn s091_remote_major_rejected_minor_unknown_fields_and_features_do_not_enable_behavior() {
    let old: Capabilities = serde_json::from_str(OLD).unwrap();
    let selected = negotiation::negotiate(&old).unwrap();
    selected.require("uploads").unwrap();
    let mut future: serde_json::Value = serde_json::from_str(OLD).unwrap();
    future["protocol"]["minor"] = 65535.into();
    future["future_field"] = serde_json::json!({"must_not_execute":true});
    future["features"]
        .as_array_mut()
        .unwrap()
        .push("erase_all_local_data".into());
    let value: Capabilities = serde_json::from_value(future.clone()).unwrap();
    let selected = negotiation::negotiate(&value).unwrap();
    assert!(selected.require("erase_all_local_data").is_err());
    assert!(
        selected
            .features()
            .all(|f| negotiation::KNOWN_FEATURES.contains(&f))
    );
    future["protocol"]["major"] = 2.into();
    assert_eq!(
        negotiation::negotiate(&serde_json::from_value(future).unwrap()).unwrap_err(),
        Error::Major { received: 2 }
    );
    let mut no_features = old;
    no_features.features.clear();
    assert!(
        negotiation::negotiate(&no_features)
            .unwrap()
            .require("uploads")
            .is_err()
    );
}
