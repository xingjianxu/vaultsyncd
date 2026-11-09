//! S102：保留真实旧设备状态，逐项记录判据；任何未触发故障/缺少重建证明均失败。
use super::*;
use sqlx::{Connection, Row, sqlite::SqliteConnectOptions};

const CREDENTIAL_NAMESPACE: &str = "51020000000000000000000000000002";
const DIRTY: &str = "S102 unacknowledged local bytes must survive\n";

pub(super) fn execute(options: &Options) -> i32 {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .expect("runtime");
    match runtime.block_on(round(options)) {
        Ok(report) => {
            fs::write(
                &options.report,
                serde_json::to_vec_pretty(&report).expect("report JSON"),
            )
            .expect("report file");
            if report["passed"] == true { 0 } else { 1 }
        }
        Err(error) => {
            eprintln!("check=fail s102 infrastructure {error}");
            let _ = fs::write(
                &options.report,
                serde_json::to_vec_pretty(
                    &json!({"passed":false,"infrastructure_error":error.to_string()}),
                )
                .unwrap(),
            );
            1
        }
    }
}
async fn control(run: &Run, action: &str) -> Result<serde_json::Value> {
    // This is the test controller, not a product capability. Keep the strict production
    // ApiClient negotiation untouched; use a separate TLS-validated, credential-free client.
    let client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(&fs::read(
            &run.proxy.ca_file,
        )?)?)
        .timeout(Duration::from_secs(120))
        .build()?;
    Ok(client
        .get(format!("{}/__s102/{action}", run.proxy.url()))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}
async fn db(client: &Client) -> Result<serde_json::Value> {
    if client.child.is_some() {
        return Err("read DB only after stopping agent".into());
    }
    let options = SqliteConnectOptions::new()
        .filename(client.state.join("client.sqlite"))
        .read_only(true);
    let mut connection = sqlx::SqliteConnection::connect_with(&options).await?;
    let mut result = serde_json::Map::new();
    for (name, query) in [
        ("epoch", "SELECT server_epoch FROM bindings"),
        ("received", "SELECT received_seq FROM bindings"),
        ("applied", "SELECT applied_seq FROM bindings"),
        (
            "baseline",
            "SELECT count(*) FROM baseline_entries WHERE deleted=0",
        ),
        ("resyncs", "SELECT count(*) FROM binding_resyncs"),
        ("quarantined", "SELECT count(*) FROM resync_quarantined_ops"),
        ("pending", "SELECT count(*) FROM pending_ops"),
        (
            "dirty",
            "SELECT count(*) FROM local_entries WHERE dirty<>'clean'",
        ),
    ] {
        let value: i64 = sqlx::query_scalar(query).fetch_one(&mut connection).await?;
        result.insert(name.into(), json!(value.to_string()));
    }
    let journals: Vec<String> =
        sqlx::query_scalar("SELECT journal FROM binding_resyncs ORDER BY resync_id")
            .fetch_all(&mut connection)
            .await?;
    result.insert("journals".into(), json!(journals));
    let operations =
        sqlx::query("SELECT lower(hex(op_id)) AS op,kind,last_error_code FROM pending_ops")
            .fetch_all(&mut connection)
            .await?;
    result.insert("operations".into(), json!(operations.iter().map(|r| json!({"op":r.get::<String,_>("op"),"kind":r.get::<String,_>("kind"),"error":r.get::<Option<String>,_>("last_error_code")})).collect::<Vec<_>>()));
    connection.close().await?;
    Ok(result.into())
}
fn classified(observed: &serde_json::Value, path: &str, expected: &str) -> bool {
    observed["database"]["journals"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .filter_map(|v| serde_json::from_str::<serde_json::Value>(v).ok())
        .any(|journal| {
            journal["snapshot_complete"] == true
                && journal["items"].as_array().is_some_and(|items| {
                    items
                        .iter()
                        .any(|item| item["path"] == path && item["disposition"] == expected)
                })
        })
}
fn saved_log(run: &Run, index: usize, phase: &str) -> Result<()> {
    fs::copy(
        &run.clients[index].log,
        run.run_dir
            .join("logs")
            .join(format!("{}-{phase}.log", run.clients[index].spec.id)),
    )?;
    Ok(())
}
async fn observe(run: &mut Run, index: usize, phase: &str) -> Result<serde_json::Value> {
    run.start_client(index).await?;
    // Not a success wait: allow repeated production cycles, then inspect actual durable state.
    tokio::time::sleep(Duration::from_secs(8)).await;
    let status = request(&CommandName::GetStatus, None).await?;
    let problems = request(&CommandName::ListProblems, None).await?;
    run.stop_client(index).await?;
    saved_log(run, index, phase)?;
    Ok(json!({"status":status,"problems":problems,"database":db(&run.clients[index]).await?}))
}
async fn delete(run: &Run, name: &str) -> Result<()> {
    let entry = snapshot_entry(&run.entries().await?, name)?;
    let (snapshot, _) = sync::snapshot(&run.api, &run.vault).await?;
    let operation = serde_json::from_value(
        json!({"op_id":id32(),"device_id":id32(),"server_epoch":snapshot.server_epoch,"kind":"delete","entry_id":entry.entry_id,"base_revision":entry.revision,"local_generation":"1"}),
    )?;
    let result = sync::submit(&run.api, &run.vault, &operation).await?;
    if result.state != OperationState::Committed {
        return Err(format!("delete not committed: {result:?}").into());
    }
    Ok(())
}
async fn fresh_owner(run: &mut Run) -> Result<()> {
    let issued: SessionIssued = run.api.send_json(ApiRequest::post("/v1/auth/login").with_json(&json!({"login":run.fixture.login,"password":run.fixture.password,"device_label":"S102 restored controller"}))?).await?;
    run.api.auth().install(Credentials::from_issued(&issued)?);
    Ok(())
}
async fn round(options: &Options) -> Result<serde_json::Value> {
    unsafe {
        AllocConsole();
    }
    let mut fixture: Fixture = serde_json::from_slice(&fs::read(&options.fixture)?)?;
    let run_dir = options
        .driver
        .join(format!("run-e2e-{}", std::process::id()));
    fs::create_dir_all(run_dir.join("logs"))?;
    let proxy = Proxy::start(&fixture.url, &run_dir.join("tls")).await?;
    fixture.client_server_id = Some(CREDENTIAL_NAMESPACE.into());
    fixture.url = proxy.url();
    fixture.ca_file = Some(proxy.ca_file.clone());
    let api = ApiClient::new(
        ClientConfig::new(proxy.url())
            .with_trust(TlsTrust::system_roots_with_pem_file(&proxy.ca_file)?),
    )?;
    api.auth()
        .install(Credentials::from_issued(&fixture.issued)?);
    let mut clients = Vec::new();
    for id in ["events", "cursor", "history", "rollback"] {
        clients.push(prepare_client(
            &run_dir,
            &ClientSpec {
                id: id.into(),
                label: format!("S102 {id}"),
                mode: Mode::Full,
            },
        )?);
    }
    let mut run = Run {
        api,
        vault: fixture.vault.clone(),
        proxy,
        fixture: fixture.clone(),
        agent: options.agent.clone(),
        run_dir,
        clients,
    };
    let outcome = matrix(&mut run).await;
    // Always stop our processes and unregister only our four CFAPI vaults; no other lab state.
    let cleanup = run.cleanup(&fixture.vault);
    erase_test_credential(CREDENTIAL_NAMESPACE);
    let mut report = outcome?;
    report["cleanup_warnings"] = json!(cleanup);
    if !cleanup.is_empty() {
        report["passed"] = json!(false);
    }
    Ok(report)
}
async fn matrix(run: &mut Run) -> Result<serde_json::Value> {
    let mut initial = Vec::new();
    for index in 0..run.clients.len() {
        run.sync_client(index).await?;
        saved_log(run, index, "initial")?;
        for seed in &run.fixture.seeded {
            if hash_of(&fs::read(run.clients[index].vault.join(&seed.path))?) != seed.hash {
                return Err("initial native hydration mismatch".into());
            }
        }
        initial.push(db(&run.clients[index]).await?);
    }
    let initial_dirty = snapshot_entry(&run.entries().await?, "dirty.txt")?;
    let backup = control(run, "backup").await?;
    let mut scenes = Vec::new();
    // Separate arms. Each requires a real product SSE request, an actual transformed hint,
    // and durable changes-driven convergence; polling alone cannot satisfy the first two.
    for mode in ["loss", "duplicate", "reorder"] {
        run.proxy.arm_sse(mode);
        run.start_client(0).await?;
        let first = control(run, &format!("event-{mode}-first")).await?;
        run.wait_idle(0).await?;
        let second = control(run, &format!("event-{mode}-second")).await?;
        run.wait_idle(0).await?;
        let end = Instant::now() + Duration::from_secs(10);
        while !run.proxy.sse_effects().iter().any(|v| v == mode) && Instant::now() < end {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        run.stop_client(0).await?;
        saved_log(run, 0, mode)?;
        let injected = run.proxy.sse_effects().iter().any(|v| v == mode);
        let content = fs::read(run.clients[0].vault.join("event.txt"))?;
        let rows = run.entries().await?;
        let event = snapshot_entry(&rows, "event.txt")?;
        let database = db(&run.clients[0]).await?;
        let converged = event.hash == Some(hash_of(&content))
            && content == format!("S102 remote event-{mode}-second\n").as_bytes()
            && database["received"] == database["applied"]
            && database["received"] != initial[0]["received"];
        scenes.push(json!({"id":format!("T14-{mode}"),"passed":injected && converged,"checks":{"real_sse_fault_fired":injected,"durable_changes_converged":converged},"remote_writes":[first,second],"database":database}));
    }
    // Queue the rollback device before expiry, while all its baseline files still exist.
    run.proxy.block_before_forward(Some("delete"));
    run.start_client(3).await?;
    fs::remove_file(run.clients[3].vault.join("pending-delete.txt"))?;
    let end = Instant::now() + Duration::from_secs(30);
    while run.proxy.blocked_requests().is_empty() && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    run.stop_client(3).await?;
    saved_log(run, 3, "delete-queued")?;
    let pending_before = db(&run.clients[3]).await?;
    let blocked = run.proxy.blocked_requests();
    run.proxy.block_before_forward(None);
    if blocked.is_empty() {
        return Err("real old Delete never reached pre-forward fault".into());
    }
    // Keep two deliberately old baseline DBs untouched while the server commits deletes.
    write_file(&run.clients[1].vault, "dirty.txt", DIRTY)?;
    write_file(&run.clients[2].vault, "dirty.txt", DIRTY)?;
    write_file(&run.clients[3].vault, "dirty.txt", DIRTY)?;
    delete(run, "clean.txt").await?;
    delete(run, "dirty.txt").await?;
    let cursor_fixture = control(run, "expire_cursor").await?;
    let audit_before = run.proxy.audit().len();
    run.proxy.block_snapshots(true);
    let outage = observe(run, 1, "rebuild-network-outage").await?;
    run.proxy.block_snapshots(false);
    let observed = observe(run, 1, "cursor-expired").await?;
    let restarted = observe(run, 1, "cursor-restart").await?;
    let rows = run.entries().await?;
    let snapshot = path_map(&rows)?;
    let checks = json!({
        "real_cursor_410":run.proxy.audit()[audit_before..].iter().any(|(p,s)|p.contains("/changes?") && *s==410),
        "rebuild_outage_does_not_discover_old_files":run.proxy.blocked_snapshots()>0 && outage["database"]["resyncs"]!="0" && outage["database"]["pending"]=="0",
        "no_clean_revival":!snapshot.contains_key("clean.txt"),
        "dirty_bytes_retained":fs::read(run.clients[1].vault.join("dirty.txt")).ok().as_deref()==Some(DIRTY.as_bytes()),
        "rebuild_journal_persisted":observed["database"]["resyncs"]!="0",
        "old_baseline_retained":observed["database"]["baseline"]==initial[1]["baseline"],
        "clean_classified_retired":classified(&observed,"clean.txt","RetiredClean"),
        "dirty_classified_recovery":classified(&observed,"dirty.txt","RecoveryCandidate"),
        "rebuild_survives_restart":classified(&restarted,"dirty.txt","RecoveryCandidate") && restarted["database"]["resyncs"]==observed["database"]["resyncs"],
    });
    scenes.push(scene(
        "T15-cursor-expired",
        checks,
        json!({"fixture":cursor_fixture,"snapshot_connections_dropped":run.proxy.blocked_snapshots(),"outage":outage,"observed":observed,"restarted":restarted}),
    ));
    let history_fixture = control(run, "expire_history").await?;
    // Recovery listings deliberately retain expired metadata. Prove the version cannot be
    // read, rather than incorrectly requiring the metadata row to disappear.
    let history = run.trash().await?;
    let read = run
        .api
        .send(
            ApiRequest::get(format!(
                "/v1/vaults/{}/entries/{}/versions/{}/content",
                run.vault,
                initial_dirty.entry_id,
                initial_dirty
                    .version_id
                    .as_deref()
                    .ok_or("missing old version")?
            ))
            .with_auth(AuthMode::Bearer),
        )
        .await;
    let expired = matches!(&read, Err(vaultsyncd_http_client::ClientError::Api(failure)) if failure.status()==410);
    let history_read = match read {
        Ok(response) => format!("HTTP {}", response.status()),
        Err(error) => error.to_string(),
    };
    let observed = observe(run, 2, "history-expired").await?;
    let paths = path_map(&run.entries().await?)?;
    let checks = json!({"real_history_expired":expired,"no_clean_revival":!paths.contains_key("clean.txt"),"dirty_bytes_retained":fs::read(run.clients[2].vault.join("dirty.txt")).ok().as_deref()==Some(DIRTY.as_bytes()),"rebuild_journal_persisted":observed["database"]["resyncs"]!="0","old_baseline_retained":observed["database"]["baseline"]==initial[2]["baseline"],"clean_classified_retired":classified(&observed,"clean.txt","RetiredClean"),"dirty_classified_recovery":classified(&observed,"dirty.txt","RecoveryCandidate")});
    scenes.push(scene(
        "T15-history-expired",
        checks,
        json!({"fixture":history_fixture,"history":history,"fixed_version_read":history_read,"observed":observed}),
    ));

    // Restore the actual verified old archive; do not fabricate a new epoch in live SQLite.
    let restored = control(run, "restore").await?;
    fresh_owner(run).await?;
    let (restored_snapshot, _) = sync::snapshot(&run.api, &run.vault).await?;
    let before = run.proxy.events().len();
    let observed = observe(run, 3, "restored-epoch").await?;
    let restarted = observe(run, 3, "restored-restart").await?;
    let after = run.proxy.events();
    let replayed: Vec<_> = after[before..]
        .iter()
        .filter(|e| e.kind == "delete")
        .collect();
    let paths = path_map(&run.entries().await?)?;
    let checks = json!({
        "real_restore_new_epoch":restored_snapshot.server_epoch!=initial[3]["epoch"].as_str().unwrap_or(""),
        "genuine_pending_delete":pending_before["pending"]!="0" && blocked.iter().any(|v|v["kind"]=="delete"),
        "no_old_epoch_delete_sent":replayed.is_empty(),
        "restored_file_not_deleted":paths.contains_key("pending-delete.txt"),
        "dirty_bytes_retained":fs::read(run.clients[3].vault.join("dirty.txt")).ok().as_deref()==Some(DIRTY.as_bytes()),
        "old_delete_quarantined":observed["database"]["quarantined"]!="0",
        "rollback_journal_persisted":observed["database"]["resyncs"]!="0",
        "quarantine_survives_restart":restarted["database"]["quarantined"]==observed["database"]["quarantined"] && restarted["database"]["quarantined"]!="0",
    });
    scenes.push(scene("T16-old-backup",checks,json!({"restore":restored,"pending_before":pending_before,"blocked_requests":blocked,"automatic_delete_requests":replayed,"observed":observed,"restarted":restarted})));
    let passed = scenes.iter().all(|scene| scene["passed"] == true);
    for scene in &scenes {
        println!(
            "scene={} passed={} checks={}",
            scene["id"], scene["passed"], scene["checks"]
        );
    }
    Ok(
        json!({"passed":passed,"suite":"S102","commit":std::env::var("VAULTSYNCD_E2E_COMMIT").unwrap_or_default(),"dirty":std::env::var("VAULTSYNCD_E2E_DIRTY").unwrap_or_default(),"guest":guest_version(),"topology":{"physical_windows_guests":1,"concurrent_agent_processes":1,"clients":4,"filesystem":"NTFS","cfapi":true},"initial":initial,"backup":backup,"scenes":scenes,"sse_effects":run.proxy.sse_effects(),"sse_trace":run.proxy.sse_trace(),"audit":run.proxy.audit(),"work_dir":control(run,"info").await?["work_dir"]}),
    )
}
fn scene(id: &str, checks: serde_json::Value, evidence: serde_json::Value) -> serde_json::Value {
    let passed = checks
        .as_object()
        .is_some_and(|checks| !checks.is_empty() && checks.values().all(|v| v == true));
    json!({"id":id,"passed":passed,"checks":checks,"evidence":evidence})
}
