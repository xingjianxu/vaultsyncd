//! Shared command examples and executor parameter validation, independent of transport/platform.
use vaultsyncd_local_ipc::{
    commands,
    generated::{CommandName, LocalIpcErrorCode, Request},
};

#[test]
fn file_projection_round_trips_and_rejects_missing_dimensions_or_lossy_flags() {
    let sample: serde_json::Value =
        serde_json::from_str(include_str!("../../../api/examples/local-ipc.files.json")).unwrap();
    let response: vaultsyncd_local_ipc::generated::Response =
        serde_json::from_value(sample.clone()).unwrap();
    assert_eq!(serde_json::to_value(&response).unwrap(), sample);
    let row = &response.result.unwrap().management.unwrap().files.unwrap()[0];
    assert!(row.pending && row.dirty && !row.synchronized && !row.release_allowed);
    let mut invalid = sample.clone();
    invalid["result"]["management"]["files"][0]["busy"] = serde_json::json!("false");
    assert!(serde_json::from_value::<vaultsyncd_local_ipc::generated::Response>(invalid).is_err());
    let mut invalid = sample;
    invalid["result"]["management"]["files"][0]
        .as_object_mut()
        .unwrap()
        .remove("availability");
    assert!(serde_json::from_value::<vaultsyncd_local_ipc::generated::Response>(invalid).is_err());
}

#[test]
fn every_management_handler_has_a_round_trip_example_and_rejects_extra_arguments() {
    let examples: Vec<serde_json::Value> = serde_json::from_str(include_str!(
        "../../../api/examples/local-ipc.management.json"
    ))
    .unwrap();
    let mut names = std::collections::HashSet::new();
    for example in examples {
        let request: Request = serde_json::from_value(example.clone()).unwrap();
        commands::validate(&request.command).unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap(), example);
        names.insert(
            serde_json::to_value(request.command.name)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned(),
        );
        let mut invalid = example.clone();
        invalid["command"]["parameters"] = serde_json::json!({"mode":"invalid"});
        let invalid: Request = serde_json::from_value(invalid).unwrap();
        assert_eq!(
            commands::validate(&invalid.command).unwrap_err().code,
            LocalIpcErrorCode::InvalidArgument
        );
        let mut unknown = example;
        unknown["command"]["parameters"] =
            serde_json::json!({"extra":"ignored parameters are forbidden"});
        assert!(serde_json::from_value::<Request>(unknown).is_err());
    }
    assert_eq!(names.len(), 22);
}
#[test]
fn passwords_are_redacted_even_if_an_entire_generated_request_is_debugged() {
    let request:Request=serde_json::from_value(serde_json::json!({"request_id":"redact","ipc_version":{"major":1,"minor":2},"command":{"name":"login","parameters":{"username":"owner","password":"unique-transient-password"}}})).unwrap();
    assert!(!format!("{request:?}").contains("unique-transient-password"));
    assert!(format!("{request:?}").contains("REDACTED"));
    assert_eq!(request.command.name, CommandName::Login);
}
#[test]
fn canonical_ids_and_counts_do_not_accept_lossy_or_ambiguous_inputs() {
    let mut request:Request=serde_json::from_value(serde_json::json!({"request_id":"bounds","ipc_version":{"major":1,"minor":2},"command":{"name":"restore_entry","parameters":{"binding_id":"11111111111111111111111111111111","entry_id":"22222222222222222222222222222222","operation_id":"33333333333333333333333333333333","base_revision":"9007199254740993"}}})).unwrap();
    commands::validate(&request.command).unwrap();
    request.command.parameters.as_mut().unwrap().base_revision = Some("09007199254740993".into());
    assert!(commands::validate(&request.command).is_err());
    request.command.parameters.as_mut().unwrap().base_revision = Some("9223372036854775808".into());
    assert!(commands::validate(&request.command).is_err());
    request.command.parameters.as_mut().unwrap().base_revision = Some("3".into());
    request.command.parameters.as_mut().unwrap().entry_id =
        Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into());
    assert!(commands::validate(&request.command).is_err());
}
