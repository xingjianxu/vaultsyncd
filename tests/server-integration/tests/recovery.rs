//! S037 验收：在每个持久化边界强杀提交子进程，重启后必须得到可证明的旧/新结果或明确暂停（T23）。
//!
//! 每个用例都走同一条路径：父进程准备装置（工作文件 = 旧内容、对象区登记旧/新内容）→ 子进程执行
//! `commit_put`，在指定边界挂起 → 父进程 `SIGKILL` → 父进程用同一份磁盘证据执行启动恢复 → 断言
//! 结果。进程内没有共享状态，因此“重启”读到的是真正的磁盘事实。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use std::os::unix::process::ExitStatusExt;

use vaultsyncd_server_core::commit::intent::PutPhase;
use vaultsyncd_server_core::recovery::RecoveryDecision;
use vaultsyncd_server_core::recovery::startup::{AdmissionState, RecoveryOutcome};

use vaultsyncd_server_tests::{
    Boundary, EXTERNAL_CONTENT, JournalState, NEW_CONTENT, OLD_CONTENT, SEED_REVISION, StoreHandle,
    VAULT, digest_of, run_recovery, start_with_recovery,
};

fn setup(name: &str) -> (StoreHandle, PathBuf) {
    let dir = std::env::temp_dir().join(format!("vaultsyncd-s037-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let handle = StoreHandle::open(&dir, None).expect("装置目录");
    handle
        .store()
        .register_object(OLD_CONTENT)
        .expect("登记旧内容对象");
    handle
        .store()
        .register_object(NEW_CONTENT)
        .expect("登记新内容对象");
    handle
        .store()
        .seed_target(OLD_CONTENT, SEED_REVISION)
        .expect("准备旧工作文件");
    (handle, dir)
}

fn state(handle: &StoreHandle) -> JournalState {
    handle.store().journal().expect("读装置状态")
}

fn work(handle: &StoreHandle) -> Vec<u8> {
    handle
        .store()
        .work_bytes()
        .expect("读工作文件")
        .expect("工作文件存在")
}

/// 子进程跑到 `boundary` 后强杀，返回被杀信号。
fn crash_child_at(dir: &Path, boundary: Boundary) -> i32 {
    let mut child = Command::new(env!("CARGO_BIN_EXE_crash-child"))
        .arg(dir)
        .arg(boundary.as_str())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("启动提交子进程");
    let probe = StoreHandle::open(dir, None).expect("只读探测句柄");
    probe
        .store()
        .wait_for_boundary(boundary, Duration::from_secs(60))
        .expect("子进程到达注入点");
    child.kill().expect("SIGKILL 提交子进程");
    let status = child.wait().expect("回收子进程");
    status.signal().expect("子进程被信号终止")
}

/// 一个边界上的崩溃 + 重启结果。
struct Scenario {
    boundary: Boundary,
    /// 崩溃后意图阶段（`None` 表示还没有意图行）。
    phase_after_crash: Option<PutPhase>,
    /// 崩溃后已提交版本数。
    versions_after_crash: usize,
    /// 重启恢复后应产生的版本数。
    recovered_versions: usize,
    /// 重启恢复后工作文件内容。
    work_after_recovery: &'static [u8],
    /// 重启恢复后的判定（`None` 表示没有未完成意图）。
    decision: Option<RecoveryDecision>,
}

#[test]
fn crash_at_each_persistence_boundary_yields_a_provable_result() {
    let scenarios = [
        Scenario {
            boundary: Boundary::BeforeIntentPrepared,
            phase_after_crash: None,
            versions_after_crash: 0,
            recovered_versions: 0,
            work_after_recovery: OLD_CONTENT,
            decision: None,
        },
        Scenario {
            boundary: Boundary::AfterIntentPrepared,
            phase_after_crash: Some(PutPhase::Prepared),
            versions_after_crash: 0,
            recovered_versions: 1,
            work_after_recovery: NEW_CONTENT,
            decision: Some(RecoveryDecision::Redo),
        },
        Scenario {
            boundary: Boundary::BeforePublish,
            phase_after_crash: Some(PutPhase::Prepared),
            versions_after_crash: 0,
            recovered_versions: 1,
            work_after_recovery: NEW_CONTENT,
            decision: Some(RecoveryDecision::Redo),
        },
        Scenario {
            boundary: Boundary::AfterPublish,
            phase_after_crash: Some(PutPhase::Prepared),
            versions_after_crash: 0,
            recovered_versions: 1,
            work_after_recovery: NEW_CONTENT,
            decision: Some(RecoveryDecision::Complete),
        },
        Scenario {
            boundary: Boundary::BeforeAdvancePublished,
            phase_after_crash: Some(PutPhase::Prepared),
            versions_after_crash: 0,
            recovered_versions: 1,
            work_after_recovery: NEW_CONTENT,
            decision: Some(RecoveryDecision::Complete),
        },
        Scenario {
            boundary: Boundary::AfterAdvancePublished,
            phase_after_crash: Some(PutPhase::Published),
            versions_after_crash: 0,
            recovered_versions: 1,
            work_after_recovery: NEW_CONTENT,
            decision: Some(RecoveryDecision::Complete),
        },
        Scenario {
            boundary: Boundary::BeforeMetadataCommit,
            phase_after_crash: Some(PutPhase::Published),
            versions_after_crash: 0,
            recovered_versions: 1,
            work_after_recovery: NEW_CONTENT,
            decision: Some(RecoveryDecision::Complete),
        },
        Scenario {
            boundary: Boundary::AfterMetadataCommit,
            phase_after_crash: Some(PutPhase::Finalized),
            versions_after_crash: 1,
            recovered_versions: 1,
            work_after_recovery: NEW_CONTENT,
            decision: None,
        },
        Scenario {
            boundary: Boundary::BeforeAck,
            phase_after_crash: Some(PutPhase::Finalized),
            versions_after_crash: 1,
            recovered_versions: 1,
            work_after_recovery: NEW_CONTENT,
            decision: None,
        },
    ];
    assert_eq!(
        scenarios.len(),
        Boundary::all().len(),
        "每个边界都要强杀一次"
    );

    for scenario in scenarios {
        let name = format!("boundary-{}", scenario.boundary.as_str());
        let (probe, dir) = setup(&name);
        let digest_old = digest_of(OLD_CONTENT);
        let signal = crash_child_at(&dir, scenario.boundary);
        assert_eq!(
            signal,
            9,
            "{} 必须被 SIGKILL 强杀",
            scenario.boundary.as_str()
        );

        // 崩溃后的磁盘证据：意图阶段与版本数都必须是“已持久化到哪一步”的事实。
        let crashed = state(&probe);
        assert_eq!(
            crashed.intents.first().map(|intent| intent.phase),
            scenario.phase_after_crash,
            "{} 的意图阶段",
            scenario.boundary.as_str()
        );
        assert_eq!(
            crashed.versions.len(),
            scenario.versions_after_crash,
            "{} 的已提交版本数",
            scenario.boundary.as_str()
        );

        // 重启：全新的装置句柄（进程内无残留）→ 先恢复 → 再判定写入门。
        let restarted = StoreHandle::open(&dir, None).expect("重启打开装置");
        let (admission, report) =
            run_blocking(start_with_recovery(&restarted)).expect("启动恢复完成");

        assert_eq!(
            report.intents.first().map(|recovered| recovered.decision),
            scenario.decision,
            "{} 的恢复判定",
            scenario.boundary.as_str()
        );
        assert_eq!(
            state(&restarted).versions.len(),
            scenario.recovered_versions,
            "{} 恢复后不能多出或少掉版本",
            scenario.boundary.as_str()
        );
        assert_eq!(
            work(&restarted),
            scenario.work_after_recovery,
            "{} 恢复后的工作文件内容",
            scenario.boundary.as_str()
        );
        assert!(
            !report.pauses_writes(),
            "{} 可证明的现场不应停写",
            scenario.boundary.as_str()
        );
        assert_eq!(admission.state(VAULT), AdmissionState::Open);
        assert!(admission.admit_write(VAULT).is_ok());
        // 旧对象与冲突候选都必须保留：恢复不删任何不可变内容。
        let settled = state(&restarted);
        assert!(
            settled.objects.contains_key(&digest_old),
            "{} 旧对象必须保留",
            scenario.boundary.as_str()
        );
        assert!(
            settled.objects.contains_key(&digest_of(NEW_CONTENT)),
            "{} 新对象必须保留",
            scenario.boundary.as_str()
        );
        // 已提交的唯一新版本必须等于意图目标内容（没有“假成功”）。
        if scenario.recovered_versions == 1 {
            assert_eq!(settled.versions[0].digest, digest_of(NEW_CONTENT));
            assert_eq!(settled.versions[0].revision.get(), SEED_REVISION + 1);
        }
    }
}

#[test]
fn published_then_externally_rewritten_keeps_both_versions_and_commits_nothing() {
    let (probe, dir) = setup("external-rewrite");
    assert_eq!(crash_child_at(&dir, Boundary::AfterAdvancePublished), 9);
    assert_eq!(
        state(&probe).intents[0].phase,
        PutPhase::Published,
        "崩溃点：已发布、事务未提交"
    );

    // 外部程序在服务端缺席时改写了工作文件（第三种内容，既非前也非后）。
    probe
        .store()
        .external_write(EXTERNAL_CONTENT)
        .expect("外部改写");

    let restarted = StoreHandle::open(&dir, None).expect("重启打开装置");
    let (admission, report) = run_blocking(start_with_recovery(&restarted)).expect("启动恢复");
    let recovered = &report.intents[0];
    assert_eq!(
        recovered.decision,
        RecoveryDecision::Reconcile {
            reason: vaultsyncd_server_core::recovery::ReconcileReason::PublishedTargetChanged,
        }
    );
    assert!(matches!(
        recovered.outcome,
        RecoveryOutcome::Preserved { .. }
    ));
    assert!(report.needs_reconciliation());
    assert!(!report.pauses_writes(), "可解释的外部改写只阻塞该条目");
    // 三个内容都在：旧对象、发布候选（意图目标）与外部写入的工作文件，一个都没被覆盖或删除。
    let settled = state(&restarted);
    assert!(settled.versions.is_empty(), "没有发布证据不得凭空提交");
    assert!(settled.objects.contains_key(&digest_of(OLD_CONTENT)));
    assert!(settled.objects.contains_key(&digest_of(NEW_CONTENT)));
    assert_eq!(work(&restarted), EXTERNAL_CONTENT, "外部内容不得被覆盖");
    assert_eq!(
        settled.intents[0].phase,
        PutPhase::Published,
        "意图保留为协调证据"
    );
    assert_eq!(admission.state(VAULT), AdmissionState::Open);
    assert!(admission.report(VAULT).is_none(), "打开的门不保留报告");
}

#[test]
fn unreadable_target_pauses_the_vault_with_needs_recovery() {
    let (probe, dir) = setup("unreadable");
    assert_eq!(crash_child_at(&dir, Boundary::AfterIntentPrepared), 9);
    probe.store().mark_unreadable().expect("注入不可读故障");

    let restarted = StoreHandle::open(&dir, None).expect("重启打开装置");
    let (admission, report) = run_blocking(start_with_recovery(&restarted)).expect("启动恢复");
    assert!(matches!(
        report.intents[0].outcome,
        RecoveryOutcome::NeedsRecovery { .. }
    ));
    assert!(report.pauses_writes());
    assert!(report.committed_versions().is_empty());
    // 无法解释的现场必须落盘，且工作文件与对象一个都不动。
    let settled = state(&restarted);
    assert_eq!(settled.intents[0].phase, PutPhase::NeedsRecovery);
    assert!(settled.versions.is_empty());
    assert_eq!(work(&restarted), OLD_CONTENT);
    assert!(settled.objects.contains_key(&digest_of(OLD_CONTENT)));
    // 写入门：拒绝写入并保留报告（readiness 为假）。
    assert_eq!(admission.state(VAULT), AdmissionState::Paused);
    let refusal = admission.admit_write(VAULT).expect_err("暂停必须拒绝写入");
    assert_eq!(refusal.code(), "needs_recovery");
    assert!(admission.report(VAULT).is_some());
}

#[test]
fn write_admission_is_closed_until_recovery_is_recorded() {
    let (handle, _dir) = setup("admission-order");
    let mut admission = vaultsyncd_server_core::recovery::startup::WriteAdmission::new();
    assert_eq!(
        admission
            .admit_write(VAULT)
            .expect_err("先恢复再开放写入口")
            .code(),
        "recovery_pending"
    );
    let report = run_blocking(run_recovery(&handle)).expect("启动恢复");
    assert!(report.intents.is_empty(), "干净装置没有未完成意图");
    admission.record(report);
    assert_eq!(admission.state(VAULT), AdmissionState::Open);
    assert!(admission.admit_write(VAULT).is_ok());
}

/// 用当前线程 runtime 跑一个异步步骤（测试是同步的：需要在同一线程里 spawn/kill 子进程）。
fn run_blocking<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}
