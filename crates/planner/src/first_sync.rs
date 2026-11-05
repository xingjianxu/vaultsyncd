//! 首次合并规划（`docs/design.md` §9.2、C22、I07）。
//!
//! 首次合并是**没有共同基线**时的入口：两端各自已有内容，要在不删除任何一侧的前提下合并。
//! 与 [`crate::reconcile`] 的区别是结构性的——本模块的入口不接受基线参数，因此“首次合并”无法拿
//! 旧基线当证据；反过来，**曾经同步过、只是游标过期的设备不得调用本入口**：那属于 design §9.2
//! 末段的“重建”，必须携带旧基线走 [`crate::reconcile`]，否则历史上的删除会被当成“另一端缺失”
//! 而复活（I07 的反面）。
//!
//! 按 design §9.2 第 4—7 步，每个规范路径只有一个计划：
//!
//! | 输入 | 计划 |
//! | --- | --- |
//! | 仅本地存在 | [`Action::UploadNewEntry`]：在服务器新建条目，条件是路径仍为空 |
//! | 仅远端存在 | [`Action::CreateFromRemote`]：按 Binding 模式建占位或下载完整内容 |
//! | 同路径、类型与内容相同 | [`Action::BindRemoteEntry`]：绑定远端 `EntryId`，不传输内容 |
//! | 同类型但内容不同 | [`Action::PreserveBoth`]`(ContentDiffers)`：服务器条目保留名称，本地另成冲突条目 |
//! | 同路径不同类型 | [`Action::PreserveBoth`]`(KindDiffers)`：保留双方类型，本地一方另名并告知用户 |
//!
//! “内容相同”只按长度与 SHA-256 判定（[`EntryKind`]），不看名称相似度或 `mtime`。任何输入都不产生
//! 删除动作（I07、C22），本地未确认内容一律保留（I02）。两侧都缺失不是计划而是错误
//! （[`PlanError::NoFacts`]）：导入清单与远端列表不一致时不能凭空造计划，更不能据此删除。

use crate::reconcile::{
    Action, DeferReason, Dependency, EntryKind, LocalFact, LocalProtection, Plan, PlanError,
    Prerequisite, ServerFact, deferred,
};

/// Binding 的本地存储模式（design C34 的 `full`/`on_demand`），只影响“仅远端存在”的条目怎样落到
/// 本地。
///
/// 客户端存储落地 Binding 配置时把该模式映射到这个枚举；不要为同一概念保留第二套拼写。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingMode {
    /// 完整双向同步：远端独有的文件必须下载完整内容后才能呈现（I05），不把半个文件当完整版本。
    Full,
    /// 按需双向同步：远端独有的文件先建立占位（只有元数据），内容按需下载。
    OnDemand,
}

/// 首次合并里“同路径不能证明同一结果”的两类原因（design §9.2 第 6—7 步、§10.1）。
///
/// 两类都保留双方：服务器已有条目保留名称，本地内容另成条目；名称预算与稳定副本身份仍归冲突规则
/// （design §10.2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreserveReason {
    /// 类型相同但内容不同：服务器条目保留当前名称，本地内容成为新冲突条目。
    ContentDiffers,
    /// 同路径不同类型（文件与目录同名）：保留双方类型，本地一方另名，并告知用户；不递归覆盖。
    KindDiffers,
}

/// 规划一个规范路径上的首次合并（design §9.2 第 4—7 步）。
///
/// `local` 与 `remote` 必须是**同一规范路径**上的两侧事实，`None` 表示该侧没有这个路径；调用方按
/// 导入清单逐路径配对调用，本函数不枚举同级条目、不读字节、不读时钟。
///
/// `mode` 只影响“仅远端存在”的条目：`Full` 要求先下载完整内容，`OnDemand` 只建占位。其余分支与
/// 模式无关，计划里也不记录它。
///
/// # Errors
///
/// - [`PlanError::NoFacts`]：两侧都没有这个路径，没有可比对的条目，不编造计划（也不推导删除）；
/// - [`PlanError::PathMismatch`]：两侧路径不同，说明调用方没有按规范路径配对事实。
pub fn plan_first_sync(
    local: Option<&LocalFact>,
    remote: Option<&ServerFact>,
    mode: BindingMode,
) -> Result<Plan, PlanError> {
    match (local, remote) {
        (None, None) => Err(PlanError::NoFacts),
        (Some(local), None) => Ok(upload_new_entry(local)),
        (None, Some(remote)) => Ok(create_from_remote(remote, mode)),
        (Some(local), Some(remote)) => {
            if local.path != remote.path {
                return Err(PlanError::PathMismatch);
            }
            Ok(merge_existing(local, remote))
        }
    }
}

/// 仅本地存在：在服务器新建条目（design §9.2 第 5 步）。没有基线修订可携带，创建条件是该路径在
/// 服务器仍为空（design §9.2 第 8 步的 CAS）。
fn upload_new_entry(local: &LocalFact) -> Plan {
    let mut dependencies = vec![Dependency::ParentDirectoryReady];
    let protection = match local.kind {
        EntryKind::File(_) => {
            dependencies.push(Dependency::SnapshotLocalContent {
                generation: local.generation,
            });
            LocalProtection::UploadSnapshot {
                generation: local.generation,
            }
        }
        // 目录没有文件内容，只需建条目；本地目录本身仍不得被改动。
        EntryKind::Directory => LocalProtection::KeepLocalContent,
    };
    Plan {
        path: local.path.clone(),
        action: Action::UploadNewEntry {
            generation: local.generation,
        },
        prerequisites: vec![
            Prerequisite::GenerationUnchanged {
                expected: local.generation,
            },
            Prerequisite::TargetPathUsable,
            Prerequisite::ServerPathVacant,
        ],
        dependencies,
        local_protection: protection,
    }
}

/// 仅远端存在：按 Binding 模式在本地建立该条目（design §9.2 第 5 步）。
///
/// 本地必须仍然没有这个路径：扫描之后新建的本地内容不能被远端占位或下载覆盖（I02）。
fn create_from_remote(remote: &ServerFact, mode: BindingMode) -> Plan {
    let mut dependencies = vec![Dependency::ParentDirectoryReady];
    if mode == BindingMode::Full {
        if let EntryKind::File(_) = remote.kind {
            // 完整模式必须先把内容下载并通过校验（I05）；给不出版本就无法满足该承诺。
            let Some(version) = remote.version else {
                return deferred(remote.path.clone(), DeferReason::IncompleteFacts);
            };
            dependencies.push(Dependency::DownloadVersion { version });
        }
    }
    Plan {
        path: remote.path.clone(),
        action: Action::CreateFromRemote {
            entry_id: remote.entry_id,
            revision: remote.revision,
            version: remote.version,
            mode,
        },
        prerequisites: vec![
            Prerequisite::BaseRevisionCurrent {
                expected: remote.revision,
            },
            Prerequisite::TargetPathUsable,
            Prerequisite::LocalPathVacant,
        ],
        dependencies,
        local_protection: LocalProtection::NotNeeded,
    }
}

/// 两侧都存在：内容相同就绑定远端身份（design §9.2 第 4 步），否则保留双方（第 6—7 步）。
fn merge_existing(local: &LocalFact, remote: &ServerFact) -> Plan {
    if local.kind == remote.kind {
        return bind_remote_entry(local, remote);
    }
    let reason = match (&local.kind, &remote.kind) {
        (EntryKind::File(_), EntryKind::File(_)) => PreserveReason::ContentDiffers,
        _ => PreserveReason::KindDiffers,
    };
    preserve_both(local, remote, reason)
}

/// 同路径、类型与内容都相同：绑定远端 `EntryId` 并写入基线，不传输内容（design §9.2 第 4 步）。
fn bind_remote_entry(local: &LocalFact, remote: &ServerFact) -> Plan {
    Plan {
        path: local.path.clone(),
        action: Action::BindRemoteEntry {
            entry_id: remote.entry_id,
            revision: remote.revision,
            version: remote.version,
        },
        prerequisites: vec![
            Prerequisite::BaseRevisionCurrent {
                expected: remote.revision,
            },
            Prerequisite::GenerationUnchanged {
                expected: local.generation,
            },
        ],
        dependencies: Vec::new(),
        local_protection: LocalProtection::KeepLocalContent,
    }
}

/// 同路径无法证明同一结果：双方都保留，本地内容不得被覆盖或删除（I02、I07）。
///
/// 服务器已有条目保留名称；本地内容另成条目。名称预算、稳定副本身份与上传快照归冲突规则
/// （design §10.1—10.2），因此这里不声明依赖。
fn preserve_both(local: &LocalFact, remote: &ServerFact, reason: PreserveReason) -> Plan {
    Plan {
        path: local.path.clone(),
        action: Action::PreserveBoth(reason),
        prerequisites: vec![
            Prerequisite::BaseRevisionCurrent {
                expected: remote.revision,
            },
            Prerequisite::GenerationUnchanged {
                expected: local.generation,
            },
        ],
        dependencies: Vec::new(),
        local_protection: LocalProtection::KeepLocalContent,
    }
}

#[cfg(test)]
mod tests {
    use super::{BindingMode, PreserveReason, plan_first_sync};
    use crate::reconcile::{
        Action, ContentFingerprint, DeferReason, Dependency, LocalFact, LocalProtection, Plan,
        PlanError, Prerequisite, ServerFact, reconcile,
    };
    use vaultsyncd_domain::ids::{EntryId, VersionId};
    use vaultsyncd_domain::version::{Generation, Revision, UtcTimestamp};
    use vaultsyncd_fs_common::path::RelativePath;

    const ENTRY: EntryId = EntryId::from_bytes([0x11; 16]);
    const VERSION: VersionId = VersionId::from_bytes([0xa1; 16]);

    fn path(text: &str) -> RelativePath {
        RelativePath::parse(text).expect("测试路径合法")
    }

    fn content(digest: u8, size: u64) -> ContentFingerprint {
        ContentFingerprint::new(size, [digest; 32])
    }

    fn revision(value: u64) -> Revision {
        Revision::new(value).expect("测试修订在范围内")
    }

    fn generation(value: u64) -> Generation {
        Generation::new(value).expect("测试代际在范围内")
    }

    fn mtime(seconds: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_seconds(seconds).expect("测试时间在范围内")
    }

    fn local_file(digest: u8, size: u64, generation_value: u64) -> LocalFact {
        LocalFact::file(
            path("docs/report.txt"),
            content(digest, size),
            generation(generation_value),
        )
    }

    fn remote_file(digest: u8, size: u64, revision_value: u64) -> ServerFact {
        ServerFact::file(
            path("docs/report.txt"),
            content(digest, size),
            ENTRY,
            revision(revision_value),
            VERSION,
        )
    }

    /// 一条表驱动用例：两侧事实、模式与期望动作。
    struct Case {
        name: &'static str,
        local: Option<LocalFact>,
        remote: Option<ServerFact>,
        mode: BindingMode,
        expected: Action,
    }

    fn plan_of(case: &Case) -> Result<Plan, PlanError> {
        plan_first_sync(case.local.as_ref(), case.remote.as_ref(), case.mode)
    }

    /// 逐行核对期望动作，并确认同一输入重复计算得到完全相同的计划。
    fn check_table(cases: &[Case]) {
        for case in cases {
            let plan = plan_of(case).unwrap_or_else(|error| panic!("{}: {error}", case.name));
            assert_eq!(plan.action, case.expected, "{}", case.name);
            assert_eq!(
                plan_of(case).expect("同一输入必须仍可规划"),
                plan,
                "{} 的计划不稳定",
                case.name
            );
        }
    }

    #[test]
    fn both_sides_present_follows_design_section_9_2() {
        let cases = [
            Case {
                name: "同路径同类型同内容",
                local: Some(local_file(0xaa, 120, 7)),
                remote: Some(remote_file(0xaa, 120, 4)),
                mode: BindingMode::Full,
                expected: Action::BindRemoteEntry {
                    entry_id: ENTRY,
                    revision: revision(4),
                    version: Some(VERSION),
                },
            },
            Case {
                name: "同路径同长度不同摘要",
                local: Some(local_file(0xcc, 120, 8)),
                remote: Some(remote_file(0xdd, 120, 5)),
                mode: BindingMode::Full,
                expected: Action::PreserveBoth(PreserveReason::ContentDiffers),
            },
            Case {
                name: "同路径同摘要不同长度",
                local: Some(local_file(0xcc, 121, 8)),
                remote: Some(remote_file(0xcc, 120, 5)),
                mode: BindingMode::OnDemand,
                expected: Action::PreserveBoth(PreserveReason::ContentDiffers),
            },
            Case {
                name: "同路径都是目录",
                local: Some(LocalFact::directory(path("docs/report.txt"), generation(6))),
                remote: Some(ServerFact::directory(
                    path("docs/report.txt"),
                    ENTRY,
                    revision(3),
                )),
                mode: BindingMode::Full,
                expected: Action::BindRemoteEntry {
                    entry_id: ENTRY,
                    revision: revision(3),
                    version: None,
                },
            },
            Case {
                name: "本地文件与远端目录同名",
                local: Some(local_file(0xaa, 120, 7)),
                remote: Some(ServerFact::directory(
                    path("docs/report.txt"),
                    ENTRY,
                    revision(4),
                )),
                mode: BindingMode::Full,
                expected: Action::PreserveBoth(PreserveReason::KindDiffers),
            },
            Case {
                name: "本地目录与远端文件同名",
                local: Some(LocalFact::directory(path("docs/report.txt"), generation(7))),
                remote: Some(remote_file(0xaa, 120, 4)),
                mode: BindingMode::OnDemand,
                expected: Action::PreserveBoth(PreserveReason::KindDiffers),
            },
        ];
        check_table(&cases);
    }

    #[test]
    fn local_only_entry_is_uploaded_without_baseline_revision() {
        let local = local_file(0xcc, 120, 8);

        let plan = plan_first_sync(Some(&local), None, BindingMode::Full).expect("可规划");

        // 首次合并没有基线修订可携带：创建条件换成“服务器该路径仍为空”。
        assert_eq!(
            plan.action,
            Action::UploadNewEntry {
                generation: generation(8)
            }
        );
        assert_eq!(
            plan.prerequisites,
            [
                Prerequisite::GenerationUnchanged {
                    expected: generation(8)
                },
                Prerequisite::TargetPathUsable,
                Prerequisite::ServerPathVacant,
            ]
        );
        assert_eq!(
            plan.dependencies,
            [
                Dependency::ParentDirectoryReady,
                Dependency::SnapshotLocalContent {
                    generation: generation(8)
                },
            ]
        );
        assert_eq!(
            plan.local_protection,
            LocalProtection::UploadSnapshot {
                generation: generation(8)
            }
        );
    }

    #[test]
    fn local_only_directory_uploads_entry_without_content_snapshot() {
        let local = LocalFact::directory(path("docs/notes"), generation(5));

        let plan = plan_first_sync(Some(&local), None, BindingMode::OnDemand).expect("可规划");

        assert_eq!(
            plan.action,
            Action::UploadNewEntry {
                generation: generation(5)
            }
        );
        assert_eq!(plan.dependencies, [Dependency::ParentDirectoryReady]);
        assert_eq!(plan.local_protection, LocalProtection::KeepLocalContent);
        assert_eq!(plan.path, path("docs/notes"));
    }

    #[test]
    fn remote_only_entry_is_created_by_binding_mode() {
        let remote = remote_file(0xaa, 120, 4);

        let full = plan_first_sync(None, Some(&remote), BindingMode::Full).expect("可规划");
        assert_eq!(
            full.action,
            Action::CreateFromRemote {
                entry_id: ENTRY,
                revision: revision(4),
                version: Some(VERSION),
                mode: BindingMode::Full,
            }
        );
        assert_eq!(
            full.dependencies,
            [
                Dependency::ParentDirectoryReady,
                Dependency::DownloadVersion { version: VERSION },
            ]
        );
        assert_eq!(
            full.prerequisites,
            [
                Prerequisite::BaseRevisionCurrent {
                    expected: revision(4)
                },
                Prerequisite::TargetPathUsable,
                Prerequisite::LocalPathVacant,
            ]
        );
        assert_eq!(full.local_protection, LocalProtection::NotNeeded);

        let on_demand =
            plan_first_sync(None, Some(&remote), BindingMode::OnDemand).expect("可规划");
        assert_eq!(
            on_demand.action,
            Action::CreateFromRemote {
                entry_id: ENTRY,
                revision: revision(4),
                version: Some(VERSION),
                mode: BindingMode::OnDemand,
            }
        );
        assert_eq!(
            on_demand.dependencies,
            [Dependency::ParentDirectoryReady],
            "占位不下载内容"
        );
    }

    #[test]
    fn remote_only_directory_never_needs_a_download() {
        let remote = ServerFact::directory(path("docs/archive"), ENTRY, revision(3));

        for (mode, expected_mode) in [
            (BindingMode::Full, BindingMode::Full),
            (BindingMode::OnDemand, BindingMode::OnDemand),
        ] {
            let plan = plan_first_sync(None, Some(&remote), mode).expect("可规划");
            assert_eq!(
                plan.action,
                Action::CreateFromRemote {
                    entry_id: ENTRY,
                    revision: revision(3),
                    version: None,
                    mode: expected_mode,
                }
            );
            assert_eq!(plan.dependencies, [Dependency::ParentDirectoryReady]);
        }
    }

    #[test]
    fn remote_file_without_version_only_blocks_full_mode() {
        let remote = ServerFact {
            version: None,
            ..remote_file(0xaa, 120, 4)
        };

        let full = plan_first_sync(None, Some(&remote), BindingMode::Full).expect("可规划");
        assert_eq!(full.action, Action::Defer(DeferReason::IncompleteFacts));
        assert_eq!(full.local_protection, LocalProtection::KeepLocalContent);
        assert!(full.dependencies.is_empty());

        // 按需模式只建占位，不需要内容版本。
        let on_demand =
            plan_first_sync(None, Some(&remote), BindingMode::OnDemand).expect("可规划");
        assert_eq!(
            on_demand.action,
            Action::CreateFromRemote {
                entry_id: ENTRY,
                revision: revision(4),
                version: None,
                mode: BindingMode::OnDemand,
            }
        );
    }

    #[test]
    fn same_content_binds_remote_entry_without_transfer() {
        let local = local_file(0xaa, 120, 7).with_observed_mtime(mtime(1_700_000_000));
        let remote = remote_file(0xaa, 120, 4).with_observed_mtime(mtime(1_500_000_000));

        let plan =
            plan_first_sync(Some(&local), Some(&remote), BindingMode::OnDemand).expect("可规划");

        assert_eq!(
            plan.action,
            Action::BindRemoteEntry {
                entry_id: ENTRY,
                revision: revision(4),
                version: Some(VERSION),
            }
        );
        assert!(plan.dependencies.is_empty(), "绑定不传输内容");
        assert_eq!(plan.local_protection, LocalProtection::KeepLocalContent);
        assert_eq!(
            plan.prerequisites,
            [
                Prerequisite::BaseRevisionCurrent {
                    expected: revision(4)
                },
                Prerequisite::GenerationUnchanged {
                    expected: generation(7)
                },
            ]
        );
    }

    #[test]
    fn same_size_different_content_keeps_both_without_deleting() {
        let local = local_file(0xcc, 120, 8);
        let remote = remote_file(0xdd, 120, 5);
        assert_eq!(content(0xcc, 120).size(), content(0xdd, 120).size());
        assert_ne!(content(0xcc, 120).sha256(), content(0xdd, 120).sha256());

        let plan = plan_first_sync(Some(&local), Some(&remote), BindingMode::Full).expect("可规划");

        assert_eq!(
            plan.action,
            Action::PreserveBoth(PreserveReason::ContentDiffers)
        );
        assert_eq!(
            plan.path,
            path("docs/report.txt"),
            "服务器条目保留原名称，本地内容另成条目"
        );
        assert!(plan.dependencies.is_empty(), "副本身份与命名归冲突规则");
        assert_eq!(plan.local_protection, LocalProtection::KeepLocalContent);
    }

    #[test]
    fn kind_clash_keeps_both_kinds() {
        let local = LocalFact::directory(path("docs/report.txt"), generation(7));
        let remote = remote_file(0xaa, 120, 4);

        let plan = plan_first_sync(Some(&local), Some(&remote), BindingMode::Full).expect("可规划");

        assert_eq!(
            plan.action,
            Action::PreserveBoth(PreserveReason::KindDiffers)
        );
        assert_eq!(plan.local_protection, LocalProtection::KeepLocalContent);
        assert_eq!(
            plan.prerequisites,
            [
                Prerequisite::BaseRevisionCurrent {
                    expected: revision(4)
                },
                Prerequisite::GenerationUnchanged {
                    expected: generation(7)
                },
            ]
        );
    }

    /// 首次合并允许的动作集合：只有“新建/绑定/保留双方”，没有删除或覆盖。
    fn preserves_both_sides(action: Action) -> bool {
        match action {
            Action::UploadNewEntry { .. }
            | Action::CreateFromRemote { .. }
            | Action::BindRemoteEntry { .. }
            | Action::PreserveBoth(_)
            | Action::Defer(_) => true,
            // 无操作与“应用远端/上传本地/同结果归并”都以基线为前提，不是首次合并的结论。
            Action::NoOp
            | Action::ApplyRemote { .. }
            | Action::UploadLocal { .. }
            | Action::MergeSameResult { .. } => false,
        }
    }

    #[test]
    fn no_input_combination_plans_a_deletion() {
        let local_candidates = [
            None,
            Some(local_file(0xaa, 120, 7)),
            Some(local_file(0xcc, 120, 9)),
            Some(LocalFact::directory(path("docs/report.txt"), generation(7))),
        ];
        let remote_candidates = [
            None,
            Some(remote_file(0xaa, 120, 4)),
            Some(remote_file(0xdd, 200, 5)),
            Some(ServerFact::directory(
                path("docs/report.txt"),
                ENTRY,
                revision(5),
            )),
        ];

        for local in &local_candidates {
            for remote in &remote_candidates {
                for mode in [BindingMode::Full, BindingMode::OnDemand] {
                    let result = plan_first_sync(local.as_ref(), remote.as_ref(), mode);
                    let label = format!(
                        "local={:?} remote={:?} mode={:?}",
                        local.as_ref().map(|fact| &fact.kind),
                        remote.as_ref().map(|fact| &fact.kind),
                        mode
                    );
                    let plan = match result {
                        Ok(plan) => plan,
                        Err(error) => {
                            assert_eq!(error, PlanError::NoFacts, "{label}");
                            assert!(local.is_none() && remote.is_none(), "{label}");
                            continue;
                        }
                    };
                    assert!(preserves_both_sides(plan.action), "{label}");
                    if local.is_some() {
                        assert_ne!(
                            plan.local_protection,
                            LocalProtection::NotNeeded,
                            "{label}：本地内容必须保留"
                        );
                        assert_ne!(
                            plan.action,
                            Action::NoOp,
                            "{label}：首次合并没有基线，不能声称无操作"
                        );
                    }
                    if remote.is_some() {
                        assert!(
                            !matches!(plan.action, Action::UploadNewEntry { .. }),
                            "{label}：远端已有条目时不能只计划新建"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn repeated_batches_produce_identical_plans() {
        let fixture = [
            (
                Some(local_file(0xaa, 120, 7)),
                Some(remote_file(0xaa, 120, 4)),
            ),
            (
                Some(local_file(0xcc, 120, 8)),
                Some(remote_file(0xdd, 120, 5)),
            ),
            (Some(local_file(0xcc, 120, 9)), None),
            (None, Some(remote_file(0xbb, 64, 6))),
            (
                None,
                Some(ServerFact::directory(
                    path("docs/archive"),
                    ENTRY,
                    revision(2),
                )),
            ),
        ];
        let first: Vec<Plan> = fixture
            .iter()
            .map(|(local, remote)| {
                plan_first_sync(local.as_ref(), remote.as_ref(), BindingMode::Full).expect("可规划")
            })
            .collect();
        let second: Vec<Plan> = fixture
            .iter()
            .map(|(local, remote)| {
                plan_first_sync(local.as_ref(), remote.as_ref(), BindingMode::Full).expect("可规划")
            })
            .collect();

        assert_eq!(first, second, "同一输入多次计算必须得到同一计划");
        assert_eq!(first.len(), fixture.len(), "每个路径一个计划，顺序稳定");
    }

    #[test]
    fn missing_everywhere_and_mismatched_paths_are_errors() {
        assert_eq!(
            plan_first_sync(None, None, BindingMode::Full),
            Err(PlanError::NoFacts)
        );

        let local = local_file(0xaa, 120, 7);
        let remote = ServerFact::file(
            path("docs/other.txt"),
            content(0xaa, 120),
            ENTRY,
            revision(4),
            VERSION,
        );
        assert_eq!(
            plan_first_sync(Some(&local), Some(&remote), BindingMode::Full),
            Err(PlanError::PathMismatch)
        );
        assert!(PlanError::PathMismatch.to_string().contains("路径"));
    }

    #[test]
    fn first_sync_is_a_separate_entry_from_baseline_reconcile() {
        let local = local_file(0xaa, 120, 7);
        let remote = remote_file(0xaa, 120, 4);

        // 旧设备重建必须携带旧基线走 reconcile；无基线时它只给出转交信号。
        let handed_over = reconcile(None, Some(&local), Some(&remote)).expect("可规划");
        assert_eq!(handed_over.action, Action::Defer(DeferReason::FirstSync));

        // 首次合并入口不接受基线参数，因此不可能把“无基线”误当成“对方删除”。
        let planned =
            plan_first_sync(Some(&local), Some(&remote), BindingMode::Full).expect("可规划");
        assert_eq!(
            planned.action,
            Action::BindRemoteEntry {
                entry_id: ENTRY,
                revision: revision(4),
                version: Some(VERSION),
            }
        );
        assert_ne!(planned.action, handed_over.action);

        // 目录与文件的比较只看类型/内容，不看路径以外的东西。
        let local_directory = LocalFact::directory(path("docs"), generation(3));
        let remote_directory = ServerFact::directory(path("docs"), ENTRY, revision(2));
        let directory_plan = plan_first_sync(
            Some(&local_directory),
            Some(&remote_directory),
            BindingMode::Full,
        )
        .expect("可规划");
        assert_eq!(
            directory_plan.local_protection,
            LocalProtection::KeepLocalContent
        );
        assert!(matches!(
            directory_plan.action,
            Action::BindRemoteEntry { version: None, .. }
        ));
    }
}
