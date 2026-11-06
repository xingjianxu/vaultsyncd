use super::*;
use crate::{
    ClientConfig, Credentials,
    testing::{FakeTransport, issued, json_response},
    transport::{TransportRequest, TransportResponse},
};
use std::sync::Arc;
const ROOT: &str = "11111111111111111111111111111111";
const SNAP: &str = "22222222222222222222222222222222";
const OP: &str = "33333333333333333333333333333333";

#[tokio::test]
async fn fixed_snapshot_server_time_and_authenticated_paging() {
    let transport = Arc::new(FakeTransport::new(|request: TransportRequest| {
        Box::pin(async move {
            assert_eq!(request.header("authorization"), Some("Bearer access"));
            if request.url().contains("entries") {
                assert!(request.url().ends_with("entries?cursor=7"));
                Ok(json_response(
                    200,
                    &SnapshotPage {
                        snapshot_id: SNAP.into(),
                        high_water_seq: "3".into(),
                        entries: vec![],
                        next_cursor: None,
                    },
                ))
            } else {
                Ok(TransportResponse::new(
                    201,
                    vec![("date".into(), "Thu, 01 Jan 1970 00:00:17 GMT".into())],
                    serde_json::to_vec(&Snapshot {
                        root_id: ROOT.into(),
                        snapshot_id: SNAP.into(),
                        server_epoch: "1".into(),
                        high_water_seq: "3".into(),
                        expires_at: "317".into(),
                        entry_count: "0".into(),
                    })
                    .unwrap(),
                ))
            }
        })
    }));
    let client = ApiClient::with_transport_for_unit_tests(
        ClientConfig::new("https://server.invalid"),
        transport.clone(),
    )
    .unwrap();
    client
        .auth()
        .install(Credentials::from_issued(&issued("access", "refresh")).unwrap());
    let (snap, time) = snapshot(&client, ROOT).await.unwrap();
    assert_eq!(time, 17);
    let page = snapshot_page(&client, ROOT, &snap.snapshot_id, Some("7"))
        .await
        .unwrap();
    assert_eq!(page.high_water_seq, "3");
    assert!(
        snapshot_page(&client, ROOT, SNAP, Some("07"))
            .await
            .is_err()
    );
    assert_eq!(transport.call_count(), 2);
}

#[tokio::test]
async fn malformed_pending_and_foreign_results_never_become_success() {
    let transport = Arc::new(FakeTransport::new(|_| {
        Box::pin(async {
            Ok(json_response(
                202,
                &OperationResult {
                    op_id: OP.into(),
                    state: OperationState::Pending,
                    entry_id: Some(ROOT.into()),
                    revision: None,
                    root_seq: None,
                    version_id: None,
                    conflict: None,
                    error_code: None,
                    error_message: None,
                    echo_local_generation: None,
                    replayed: None,
                },
            ))
        })
    }));
    let client = ApiClient::with_transport_for_unit_tests(
        ClientConfig::new("https://server.invalid"),
        transport,
    )
    .unwrap();
    client
        .auth()
        .install(Credentials::from_issued(&issued("access", "refresh")).unwrap());
    assert!(operation(&client, ROOT, OP).await.is_err());
    assert!(operation(&client, ROOT, SNAP).await.is_err());
}
