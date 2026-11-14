//! Shared command examples and executor parameter validation, independent of transport/platform.
use vaultsyncd_local_ipc::{
    commands,
    generated::{CommandName, LocalIpcErrorCode, PROTOCOL_MAJOR, PROTOCOL_MINOR, Request},
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
        assert!(vaultsyncd_local_ipc::version::is_compatible(
            &request.ipc_version
        ));
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
    assert_eq!(names.len(), 30);
}
#[test]
fn passwords_are_redacted_even_if_an_entire_generated_request_is_debugged() {
    let request:Request=serde_json::from_value(serde_json::json!({"request_id":"redact","ipc_version":{"major":PROTOCOL_MAJOR,"minor":PROTOCOL_MINOR},"command":{"name":"login","parameters":{"username":"owner","password":"unique-transient-password"}}})).unwrap();
    assert!(!format!("{request:?}").contains("unique-transient-password"));
    assert!(format!("{request:?}").contains("REDACTED"));
    assert_eq!(request.command.name, CommandName::Login);
}
#[test]
fn canonical_ids_and_counts_do_not_accept_lossy_or_ambiguous_inputs() {
    let mut request:Request=serde_json::from_value(serde_json::json!({"request_id":"bounds","ipc_version":{"major":PROTOCOL_MAJOR,"minor":PROTOCOL_MINOR},"command":{"name":"restore_entry","parameters":{"binding_id":"11111111111111111111111111111111","entry_id":"22222222222222222222222222222222","operation_id":"33333333333333333333333333333333","base_revision":"9007199254740993"}}})).unwrap();
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

#[test]
fn extra_exclusion_rules_accept_empty_text_but_refuse_overflow_and_empty_version() {
    let mut command: vaultsyncd_local_ipc::generated::Command=serde_json::from_value(serde_json::json!({"name":"set_rules","parameters":{"binding_id":"a5".repeat(16),"rules_version":"s105","rules":""}})).unwrap();
    commands::validate(&command).unwrap();
    command.parameters.as_mut().unwrap().rules = Some("x".repeat(4097));
    assert!(commands::validate(&command).is_err());
    command.parameters.as_mut().unwrap().rules = Some(String::new());
    command.parameters.as_mut().unwrap().rules_version = Some(String::new());
    assert!(commands::validate(&command).is_err());
}

#[test]
fn discovery_only_accepts_an_address_and_optional_trust_without_identity_or_credentials() {
    use vaultsyncd_local_ipc::generated::{Command, CommandParameters as P};
    let mut command = Command {
        name: CommandName::DiscoverConnection,
        parameters: Some(P {
            server_url: Some("https://sync.example".into()),
            ..Default::default()
        }),
    };
    commands::validate(&command).unwrap();
    for (field, value) in [
        ("server_id", "a5".repeat(16)),
        ("username", "owner".into()),
        ("password", "secret".into()),
    ] {
        let mut wire = serde_json::to_value(&command).unwrap();
        wire["parameters"][field] = value.into();
        assert!(commands::validate(&serde_json::from_value(wire).unwrap()).is_err());
    }
    command.parameters.as_mut().unwrap().server_url = None;
    assert!(commands::validate(&command).is_err());
}

#[test]
fn activity_cursors_are_positive_decimal_and_filters_are_exact() {
    use vaultsyncd_local_ipc::generated::{Command, CommandParameters as P};
    let mut command = Command {
        name: CommandName::ListActivity,
        parameters: Some(P {
            binding_id: Some("11".repeat(16)),
            cursor: Some("9007199254740993".into()),
            direction: Some("download".into()),
            ..Default::default()
        }),
    };
    commands::validate(&command).unwrap();
    for cursor in [
        "0",
        "01",
        "-1",
        "+1",
        " 1",
        "1.0",
        "1e2",
        "9223372036854775808",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        command.parameters.as_mut().unwrap().cursor = Some(cursor.into());
        assert!(
            commands::validate(&command).is_err(),
            "accepted cursor {cursor}"
        );
    }
    command.parameters.as_mut().unwrap().cursor = Some(i64::MAX.to_string());
    commands::validate(&command).unwrap();
    command.parameters.as_mut().unwrap().direction = Some("upload".into());
    commands::validate(&command).unwrap();
    command.parameters.as_mut().unwrap().direction = Some("operation".into());
    commands::validate(&command).unwrap();
    for direction in ["", "UPLOAD", "Operation", "both", "upload&before=1"] {
        command.parameters.as_mut().unwrap().direction = Some(direction.into());
        assert!(commands::validate(&command).is_err());
    }
    command.parameters.as_mut().unwrap().direction = None;
    command.parameters.as_mut().unwrap().cursor = None;
    commands::validate(&command).unwrap();
    command.parameters.as_mut().unwrap().binding_id = None;
    assert!(commands::validate(&command).is_err());
}

#[test]
fn activity_records_round_trip_with_lossless_counts_and_require_observed_facts() {
    use vaultsyncd_local_ipc::generated::ManagementResult;
    let sample = serde_json::json!({"activity":[{
        "id":"9007199254740993", "occurred_at":"1794300000", "client_label":"客体A", "client_kind":"native",
        "direction":"download", "action":"download", "status":"served", "path":"文件.txt",
        "size":"9007199254740993", "bytes":"4", "offset":"9007199254740989"
    }],"next_cursor":"9007199254740993"});
    let result: ManagementResult = serde_json::from_value(sample.clone()).unwrap();
    assert_eq!(serde_json::to_value(result).unwrap(), sample);
    let mut missing = sample;
    missing["activity"][0]
        .as_object_mut()
        .unwrap()
        .remove("status");
    assert!(serde_json::from_value::<ManagementResult>(missing).is_err());
}

#[test]
fn server_scopes_are_canonical_and_cannot_override_a_bindings_server() {
    use vaultsyncd_local_ipc::generated::{Command, CommandParameters as P};
    let mut command = Command {
        name: CommandName::ListVaults,
        parameters: Some(P {
            server_id: Some("a5".repeat(16)),
            ..Default::default()
        }),
    };
    commands::validate(&command).unwrap();
    command.parameters.as_mut().unwrap().server_id = Some("A5".repeat(16));
    assert!(commands::validate(&command).is_err());
    command.parameters.as_mut().unwrap().server_id = Some("a5".repeat(16));
    command.parameters.as_mut().unwrap().binding_id = Some("b6".repeat(16));
    command.name = CommandName::ListFiles;
    assert!(commands::validate(&command).is_err());
}
