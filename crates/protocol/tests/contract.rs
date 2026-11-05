//! 契约测试：`api/examples/` 的样本必须能被生成的 DTO 接受，非法 wire 形态必须被拒绝。
//!
//! 这些用例同时守住 S024 的三条验收：同源生成的两侧都编译（Rust 在这里编译、TypeScript 由
//! `pnpm --filter @vaultsyncd/api-client typecheck` 与 `test` 覆盖）、未知枚举不默认成功、
//! 长整数只以十进制字符串传输。样本文件是契约的一部分（design §5.6 的 `api/examples/`），
//! 因此测试直接读它们，而不是在测试里另抄一份 JSON。

use vaultsyncd_domain::ids::ServerId;
use vaultsyncd_domain::version::{BoundError, Revision, ServerEpoch};
use vaultsyncd_protocol::decimal::DecimalTextError;
use vaultsyncd_protocol::domain_map::{
    server_epoch_from_wire, server_epoch_to_wire, server_id_from_wire, server_id_to_wire,
};
use vaultsyncd_protocol::generated::{
    Capabilities, ErrorCode, ErrorEnvelope, HealthResponse, HealthStatus,
};

/// `api/examples/capabilities.response.json`
const CAPABILITIES_EXAMPLE: &str = include_str!("../../../api/examples/capabilities.response.json");
/// `api/examples/health.response.json`
const HEALTH_EXAMPLE: &str = include_str!("../../../api/examples/health.response.json");
/// `api/examples/error.response.json`
const ERROR_EXAMPLE: &str = include_str!("../../../api/examples/error.response.json");

fn capabilities() -> Capabilities {
    serde_json::from_str(CAPABILITIES_EXAMPLE).expect("capabilities 样本必须符合契约")
}

#[test]
fn capabilities_example_decodes_with_string_epoch() {
    let capabilities = capabilities();
    assert_eq!(capabilities.protocol.major, 1);
    assert_eq!(capabilities.protocol.minor, 0);
    assert_eq!(capabilities.server_id, "0123456789abcdef0123456789abcdef");
    // 2^53 + 1：JSON number 会把这个值读成 9007199254740992，只有字符串能原样传。
    assert_eq!(capabilities.epoch, "9007199254740993");
    assert_eq!(capabilities.limits.max_page_size, 500);
    assert_eq!(capabilities.limits.max_upload_part_bytes, "67108864");
}

#[test]
fn capabilities_epoch_roundtrips_through_domain_type() {
    let capabilities = capabilities();
    let epoch: ServerEpoch =
        server_epoch_from_wire(&capabilities.epoch).expect("样本 epoch 是规范十进制文本");
    assert_eq!(epoch.get(), 9_007_199_254_740_993);
    assert_eq!(server_epoch_to_wire(epoch), capabilities.epoch);
}

#[test]
fn unknown_feature_token_is_tolerated_but_kept() {
    // features 是显式协商用的可扩展标记集合，不是闭集枚举：新服务端多给的标记必须原样保留，
    // 由调用方按“认识才用”处理。
    let capabilities = capabilities();
    assert!(
        capabilities
            .features
            .iter()
            .any(|feature| feature == "feature_from_a_newer_server")
    );
}

#[test]
fn epoch_as_json_number_is_rejected() {
    let mut value: serde_json::Value =
        serde_json::from_str(CAPABILITIES_EXAMPLE).expect("样本是合法 JSON");
    value["epoch"] = serde_json::json!(9_007_199_254_740_993_u64);
    let error = serde_json::from_value::<Capabilities>(value).expect_err("epoch 必须是字符串");
    assert!(
        error.to_string().contains("invalid type"),
        "错误应指出类型不符，实际：{error}"
    );
}

#[test]
fn non_canonical_epoch_text_is_rejected() {
    for (text, expected) in [
        ("01", DecimalTextError::LeadingZero),
        ("-1", DecimalTextError::Negative),
        ("", DecimalTextError::Empty),
        ("9007199254740993 ", DecimalTextError::InvalidDigit),
        ("9223372036854775808", DecimalTextError::AboveDbLimit),
    ] {
        let mut value: serde_json::Value =
            serde_json::from_str(CAPABILITIES_EXAMPLE).expect("样本是合法 JSON");
        value["epoch"] = serde_json::json!(text);
        let error = serde_json::from_value::<Capabilities>(value).expect_err(text);
        assert!(
            error.to_string().contains(&expected.to_string()),
            "文本 `{text}` 的错误应包含 `{expected}`，实际：{error}"
        );
    }
}

#[test]
fn health_example_decodes_closed_status_enum() {
    let health: HealthResponse =
        serde_json::from_str(HEALTH_EXAMPLE).expect("health 样本必须符合契约");
    assert_eq!(health.status, HealthStatus::Ok);

    let unknown = serde_json::json!({ "status": "needs_migration" });
    let error = serde_json::from_value::<HealthResponse>(unknown).expect_err("未知状态必须失败");
    assert!(
        error.to_string().contains("unknown variant"),
        "错误应指出未知取值，实际：{error}"
    );
}

#[test]
fn error_example_decodes_and_rejects_unknown_code() {
    let envelope: ErrorEnvelope =
        serde_json::from_str(ERROR_EXAMPLE).expect("错误样本必须符合契约");
    let body = &envelope.error;
    assert_eq!(body.code, ErrorCode::RootUnavailable);
    assert!(body.retryable);
    assert_eq!(body.request_id, "req_example");
    assert_eq!(
        body.root_id.as_deref(),
        Some("fedcba9876543210fedcba9876543210")
    );
    assert_eq!(
        body.details
            .as_ref()
            .and_then(|details| details.get("reason")),
        Some(&"mount_identity_changed".to_string())
    );

    let mut value: serde_json::Value = serde_json::from_str(ERROR_EXAMPLE).expect("样本合法");
    value["error"]["code"] = serde_json::json!("LOCAL_CHANGED");
    let error = serde_json::from_value::<ErrorEnvelope>(value)
        .expect_err("本机 IPC 错误码不属于远端闭集，必须失败");
    assert!(
        error.to_string().contains("unknown variant"),
        "错误应指出未知取值，实际：{error}"
    );
}

#[test]
fn error_optional_fields_may_be_absent() {
    let minimal = serde_json::json!({
        "error": {
            "code": "RATE_LIMITED",
            "message": "请稍后重试",
            "retryable": true,
            "request_id": "req_minimal"
        }
    });
    let envelope: ErrorEnvelope =
        serde_json::from_value(minimal).expect("root_id 与 details 可省略");
    assert!(envelope.error.root_id.is_none());
    assert!(envelope.error.details.is_none());
}

#[test]
fn unknown_fields_are_tolerated_for_forward_compatibility() {
    // 旧客户端遇到新服务端多给的字段应继续工作；闭集枚举与必填字段仍然严格。
    let with_extra = serde_json::json!({
        "status": "ok",
        "server_time": "2026-11-05T00:00:00Z"
    });
    let health: HealthResponse = serde_json::from_value(with_extra).expect("未知字段应被忽略");
    assert_eq!(health.status, HealthStatus::Ok);
}

#[test]
fn domain_map_roundtrips_server_id() {
    let capabilities = capabilities();
    let id: ServerId =
        server_id_from_wire(&capabilities.server_id).expect("样本 server_id 是规范十六进制文本");
    assert_eq!(server_id_to_wire(id), capabilities.server_id);
    assert!(server_id_from_wire("not-hex").is_err());
    assert!(server_id_from_wire("0123").is_err());
}

#[test]
fn domain_map_rejects_invalid_epoch_text() {
    assert_eq!(
        server_epoch_from_wire("1.5"),
        Err(BoundError::InvalidDigit),
        "domain 转换必须拒绝小数"
    );
    assert_eq!(server_epoch_from_wire("0"), Ok(ServerEpoch::ZERO));
}

#[test]
fn decimal_wire_rules_match_domain_counter_rules() {
    // 两处规则必须一致：wire 适配器（crate::decimal）与 domain 计数解析（S015）。
    let cases = [
        "",
        "-1",
        "0",
        "42",
        "01",
        "+1",
        " 1",
        "1.0",
        "1e3",
        "9007199254740993",
        "9223372036854775807",
        "9223372036854775808",
        "99999999999999999999999999999999999999",
    ];
    for text in cases {
        let wire_ok = vaultsyncd_protocol::decimal::validate(text).is_ok();
        let domain_ok = Revision::from_decimal(text).is_ok();
        assert_eq!(wire_ok, domain_ok, "文本 `{text}` 在两处规则不一致");
    }
}

#[test]
fn serialize_writes_canonical_decimal_text() {
    let capabilities = capabilities();
    let json = serde_json::to_string(&capabilities).expect("可序列化");
    assert!(
        json.contains("\"epoch\":\"9007199254740993\""),
        "序列化必须保留字符串形态，实际：{json}"
    );
}
