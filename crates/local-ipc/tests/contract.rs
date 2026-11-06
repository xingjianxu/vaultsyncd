//! 契约测试：`api/examples/local-ipc.*.json` 的样本必须被生成的 DTO 接受并原样重编码，
//! 未知命令/未知应答种类/缺字段/截断/超长帧必须被拒绝，`get_status` 必须由处理器应答。
//!
//! 样本文件是契约的一部分（design §5.6 的 `api/examples/`），因此测试直接读它们；TypeScript
//! 侧的 `packages/local-ipc-types/tests/schema-decoders.mjs` 读同一份文件并断言同样的结论，
//! “Rust 与 TS 样本一致”因此是同一份数据通过两套生成代码，而不是两边各抄一份期望值。

use serde_json::{Value, json};
use vaultsyncd_local_ipc::framing::{self, FrameDecoder, FrameError, MAX_QUEUED_FRAMES};
use vaultsyncd_local_ipc::generated::{
    Command, CommandName, GetStatusResult, IpcVersion, LocalIpcErrorCode, MAX_FRAME_BYTES,
    PROTOCOL_MAJOR, PROTOCOL_MINOR, Request, Response, ResponseKind,
};
use vaultsyncd_local_ipc::service::{ResponseShapeError, Service};
use vaultsyncd_local_ipc::version;

/// `api/examples/local-ipc.request.json`
const REQUEST_EXAMPLE: &str = include_str!("../../../api/examples/local-ipc.request.json");
/// `api/examples/local-ipc.response.json`
const RESPONSE_EXAMPLE: &str = include_str!("../../../api/examples/local-ipc.response.json");
/// `api/examples/local-ipc.error.json`
const ERROR_EXAMPLE: &str = include_str!("../../../api/examples/local-ipc.error.json");

/// 把样本解析成通用 JSON 值，用来比对“解码后再编码”是否与原样本一致。
fn sample(text: &str) -> Value {
    serde_json::from_str(text).expect("样本必须是合法 JSON")
}

/// 构造一条 `get_status` 请求。
fn get_status(request_id: &str, major: u32, minor: u32) -> Request {
    Request {
        command: Command {
            name: CommandName::GetStatus,
            parameters: None,
        },
        ipc_version: IpcVersion { major, minor },
        request_id: request_id.to_string(),
    }
}

#[test]
fn request_example_round_trips() {
    let request: Request = serde_json::from_str(REQUEST_EXAMPLE).expect("请求样本必须符合契约");
    assert_eq!(
        serde_json::to_value(&request).expect("可序列化"),
        sample(REQUEST_EXAMPLE)
    );
}

#[test]
fn response_examples_round_trip_and_validate() {
    for text in [RESPONSE_EXAMPLE, ERROR_EXAMPLE] {
        let response: Response = serde_json::from_str(text).expect("应答样本必须符合契约");
        response.validate().expect("样本的 kind 与负载必须一致");
        assert_eq!(
            serde_json::to_value(&response).expect("可序列化"),
            sample(text)
        );
    }
}

#[test]
fn real_status_addition_preserves_decimal_counts_and_legacy_response() {
    let text = include_str!("../../../api/examples/local-ipc.status.json");
    let response: Response = serde_json::from_str(text).unwrap();
    response.validate().unwrap();
    assert_eq!(serde_json::to_value(&response).unwrap(), sample(text));
    let statuses = response.result.unwrap().bindings.unwrap();
    assert_eq!(statuses[0].dirty, "9007199254740993");
    assert!(!statuses[0].synchronized);
    let legacy: Response = serde_json::from_str(RESPONSE_EXAMPLE).unwrap();
    assert!(legacy.result.unwrap().bindings.is_none());
    let old_request: Request = serde_json::from_str(REQUEST_EXAMPLE).unwrap();
    assert!(Service::new("1.1").handle(&old_request).result.is_some());
}

#[test]
fn unknown_command_is_rejected() {
    let mut value = sample(REQUEST_EXAMPLE);
    value["command"]["name"] = json!("delete_everything_unknown");
    let error = serde_json::from_value::<Request>(value).expect_err("未知命令必须被拒绝");
    assert!(
        error.to_string().contains("unknown variant"),
        "错误应指出未知命令，实际：{error}"
    );
}

#[test]
fn unknown_response_kind_is_rejected() {
    let mut value = sample(RESPONSE_EXAMPLE);
    value["kind"] = json!("partial");
    let error = serde_json::from_value::<Response>(value).expect_err("未知应答种类必须被拒绝");
    assert!(
        error.to_string().contains("unknown variant"),
        "错误应指出未知应答种类，实际：{error}"
    );
}

#[test]
fn missing_required_field_is_rejected() {
    let mut value = sample(REQUEST_EXAMPLE);
    value
        .as_object_mut()
        .expect("样本是对象")
        .remove("request_id");
    let error = serde_json::from_value::<Request>(value).expect_err("缺必填字段必须被拒绝");
    assert!(
        error.to_string().contains("request_id"),
        "错误应指出缺失字段，实际：{error}"
    );
}

#[test]
fn version_numbers_must_be_integers() {
    let mut value = sample(REQUEST_EXAMPLE);
    value["ipc_version"]["major"] = json!("1");
    let error = serde_json::from_value::<Request>(value).expect_err("版本号必须是整数");
    assert!(
        error.to_string().contains("invalid type"),
        "错误应指出类型不符，实际：{error}"
    );
}

#[test]
fn get_status_is_answered_with_agent_identity() {
    let service = Service::new("0.1.0");
    let response = service.handle(&get_status(
        "req_example_001",
        PROTOCOL_MAJOR,
        PROTOCOL_MINOR,
    ));
    response.validate().expect("kind 与负载一致");
    assert_eq!(response.kind, ResponseKind::Ok);
    assert_eq!(response.request_id, "req_example_001");
    let result = response.result.expect("ok 必须带结果");
    assert_eq!(result.agent_version, "0.1.0");
    assert_eq!(result.ipc_protocol, version::supported());
    assert!(response.error.is_none());
}

#[test]
fn incompatible_major_version_is_rejected() {
    let service = Service::new("0.1.0");
    let response = service.handle(&get_status("req_major", PROTOCOL_MAJOR + 1, PROTOCOL_MINOR));
    response.validate().expect("kind 与负载一致");
    assert_eq!(response.kind, ResponseKind::Error);
    assert!(response.result.is_none());
    let error = response.error.expect("error 必须带错误主体");
    assert_eq!(error.code, LocalIpcErrorCode::UnsupportedVersion);
    assert!(!error.retryable, "协议不兼容不能标记为可重试");
    assert_eq!(response.request_id, "req_major");
}

#[test]
fn newer_minor_version_is_rejected() {
    let service = Service::new("0.1.0");
    let newer = service.handle(&get_status("req_newer", PROTOCOL_MAJOR, PROTOCOL_MINOR + 1));
    assert_eq!(newer.kind, ResponseKind::Error);
    assert_eq!(
        newer.error.expect("有错误主体").code,
        LocalIpcErrorCode::UnsupportedVersion
    );
}

#[test]
fn version_compatibility_follows_major_and_minor() {
    assert!(version::is_compatible(&version::supported()));
    assert!(version::is_compatible(&IpcVersion {
        major: PROTOCOL_MAJOR,
        minor: 0,
    }));
    assert!(!version::is_compatible(&IpcVersion {
        major: PROTOCOL_MAJOR + 1,
        minor: 0,
    }));
    assert!(!version::is_compatible(&IpcVersion {
        major: PROTOCOL_MAJOR,
        minor: PROTOCOL_MINOR + 1,
    }));
}

#[test]
fn framed_request_reaches_the_handler_and_response_is_framed() {
    let service = Service::new("9.9.9");
    let payload = serde_json::to_vec(&get_status("req_frame", PROTOCOL_MAJOR, PROTOCOL_MINOR))
        .expect("可序列化");
    let frame = framing::encode(&payload).expect("合法请求可编码");

    // 任意切分的读入块（含 1 字节一块）都必须得到同一帧。
    let mut decoder = FrameDecoder::new();
    for (index, byte) in frame.iter().enumerate() {
        decoder
            .push(std::slice::from_ref(byte))
            .expect("逐字节读入合法");
        let seen = index + 1;
        assert_eq!(
            decoder.queued_frames(),
            usize::from(seen == frame.len()),
            "只有最后一个字节到齐后才能产出整帧（已读 {seen} 字节）"
        );
    }
    let decoded_payload = decoder.next_frame().expect("帧已完整");
    assert_eq!(decoded_payload, payload);

    let request: Request = serde_json::from_slice(&decoded_payload).expect("请求可解码");
    let response = service.handle(&request);
    response.validate().expect("应答形状合法");

    let response_payload = serde_json::to_vec(&response).expect("可序列化");
    let response_frame = framing::encode(&response_payload).expect("应答可编码");
    let mut decoder = FrameDecoder::new();
    decoder.push(&response_frame).expect("整帧读入合法");
    let decoded: Response =
        serde_json::from_slice(&decoder.next_frame().expect("应答帧完整")).expect("应答可解码");
    decoded.validate().expect("解码后的应答形状合法");
    assert_eq!(
        decoded.result.expect("ok 结果"),
        GetStatusResult {
            agent_version: "9.9.9".to_string(),
            ipc_protocol: version::supported(),
            bindings: None,
            snapshot_version: None,
            management: None,
        }
    );
}

#[test]
fn truncated_frame_yields_no_frame() {
    let payload = serde_json::to_vec(&get_status("req_cut", PROTOCOL_MAJOR, PROTOCOL_MINOR))
        .expect("可序列化");
    let frame = framing::encode(&payload).expect("合法请求可编码");
    let mut decoder = FrameDecoder::new();
    decoder.push(&frame[..frame.len() - 1]).expect("读入合法");
    assert_eq!(decoder.next_frame(), None, "截断的帧不能被当成完整帧");
    assert!(decoder.buffered_bytes() > 0, "截断的字节留在缓冲里等待后续");
}

#[test]
fn truncated_json_payload_is_rejected_by_decoding() {
    let payload = serde_json::to_vec(&get_status("req_json", PROTOCOL_MAJOR, PROTOCOL_MINOR))
        .expect("可序列化");
    let cut = &payload[..payload.len() / 2];
    let frame = framing::encode(cut).expect("帧层只看长度，短负载也是合法帧");
    let mut decoder = FrameDecoder::new();
    decoder.push(&frame).expect("整帧读入合法");
    let decoded = decoder.next_frame().expect("帧层给出完整负载");
    serde_json::from_slice::<Request>(&decoded).expect_err("截断的 JSON 必须解码失败");
}

#[test]
fn overlong_declared_length_is_rejected() {
    let declared = u32::try_from(MAX_FRAME_BYTES + 1).expect("上限远小于 u32::MAX");
    let mut decoder = FrameDecoder::new();
    let error = decoder
        .push(&declared.to_be_bytes())
        .expect_err("超长声明必须被拒绝");
    assert_eq!(
        error,
        FrameError::FrameTooLarge {
            bytes: MAX_FRAME_BYTES + 1
        }
    );
    assert!(decoder.buffered_bytes() <= 4, "拒绝超长帧时不能先为它扩容");
}

#[test]
fn zero_length_frame_is_rejected() {
    let mut decoder = FrameDecoder::new();
    let error = decoder
        .push(&0u32.to_be_bytes())
        .expect_err("空帧必须被拒绝");
    assert_eq!(error, FrameError::EmptyFrame);
}

#[test]
fn oversized_payload_cannot_be_encoded() {
    let payload = vec![b'x'; MAX_FRAME_BYTES + 1];
    assert_eq!(
        framing::encode(&payload),
        Err(FrameError::FrameTooLarge {
            bytes: MAX_FRAME_BYTES + 1
        })
    );
    assert_eq!(framing::encode(&[]), Err(FrameError::EmptyFrame));
}

#[test]
fn queued_frames_are_bounded() {
    let payload = serde_json::to_vec(&get_status("req_queue", PROTOCOL_MAJOR, PROTOCOL_MINOR))
        .expect("可序列化");
    let frame = framing::encode(&payload).expect("合法请求可编码");
    let mut chunk = Vec::new();
    for _ in 0..=MAX_QUEUED_FRAMES {
        chunk.extend_from_slice(&frame);
    }
    let mut decoder = FrameDecoder::new();
    let error = decoder
        .push(&chunk)
        .expect_err("未取走的帧超过上限必须报错");
    assert_eq!(error, FrameError::Backlog);
}

#[test]
fn multiple_frames_are_delivered_in_order() {
    let payloads = [
        serde_json::to_vec(&get_status("req_a", PROTOCOL_MAJOR, PROTOCOL_MINOR)).expect("可序列化"),
        serde_json::to_vec(&get_status("req_b", PROTOCOL_MAJOR, PROTOCOL_MINOR)).expect("可序列化"),
    ];
    let mut chunk = Vec::new();
    for payload in &payloads {
        chunk.extend_from_slice(&framing::encode(payload).expect("可编码"));
    }
    let mut decoder = FrameDecoder::new();
    decoder.push(&chunk).expect("整块读入合法");
    let mut delivered = Vec::new();
    while let Some(frame) = decoder.next_frame() {
        delivered.push(frame);
    }
    assert_eq!(delivered, payloads);
    assert_eq!(decoder.buffered_bytes(), 0);
}

#[test]
fn response_shape_validation_catches_mismatch() {
    let response = Response {
        request_id: "req_bad".to_string(),
        kind: ResponseKind::Ok,
        result: None,
        error: None,
    };
    let error = response.validate().expect_err("ok 没有 result 必须报错");
    assert!(matches!(
        error,
        ResponseShapeError::PayloadMismatch {
            kind: ResponseKind::Ok,
            has_result: false,
            has_error: false,
        }
    ));
}
