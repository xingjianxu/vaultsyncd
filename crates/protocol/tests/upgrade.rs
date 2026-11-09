//! S091固定开发协议fixture；不冒充前一正式客户端。改名后 major 1 的冻结样本必须被明确拒绝。
use vaultsyncd_protocol::{
    generated::Capabilities,
    negotiation::{self, Error},
};
const OLD: &str = include_str!("../../../tests/contract/fixtures/s091/remote-v1.0.json");
#[test]
fn s091_remote_major_rejected_minor_unknown_fields_and_features_do_not_enable_behavior() {
    let old: Capabilities = serde_json::from_str(OLD).unwrap();
    assert_eq!(
        negotiation::negotiate(&old).unwrap_err(),
        Error::Major { received: 1 }
    );
    // 其余判定针对当前 major：旧 fixture 的 minor/未知字段/未知功能语义保持不变。
    let mut current: serde_json::Value = serde_json::from_str(OLD).unwrap();
    current["protocol"]["major"] = negotiation::MAJOR.into();
    let selected =
        negotiation::negotiate(&serde_json::from_value(current.clone()).unwrap()).unwrap();
    selected.require("uploads").unwrap();
    let mut future: serde_json::Value = serde_json::from_value(current.clone()).unwrap();
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
    future["protocol"]["major"] = (negotiation::MAJOR + 1).into();
    assert_eq!(
        negotiation::negotiate(&serde_json::from_value(future).unwrap()).unwrap_err(),
        Error::Major {
            received: negotiation::MAJOR + 1
        }
    );
    let mut no_features: Capabilities = serde_json::from_value(current).unwrap();
    no_features.features.clear();
    assert!(
        negotiation::negotiate(&no_features)
            .unwrap()
            .require("uploads")
            .is_err()
    );
}
