//! 把工作文件固定成不可变内容对象（design §11.1—11.2 步骤 2—3、I12、C37）。
//!
//! 顺序固定为：建立对象区内的私有副本 → 核对“独立 inode/无别名 → 长度 → SHA-256” →
//! 按内容地址发布（不覆盖）→ 发布后复核。任何一步失败都返回错误，绝不返回可提交引用；
//! 已建立但未发布的副本会被删除，删除失败也随错误一起报告。

use std::fmt;

use crate::objects::error::{CaptureFailure, CaptureFailureReason, DiscardOutcome};
use crate::objects::{ContentDigest, ObjectLayout};
use crate::ports::object_store::{BlobError, CopyFallback, CopyMethod, ObjectBlobPort, StagedBlob};

/// 期望内容：上传校验或采集观察给出的长度与 SHA-256。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpectedContent {
    /// 期望的 SHA-256。
    pub digest: ContentDigest,
    /// 期望的字节数。
    pub bytes: u64,
}

impl ExpectedContent {
    /// 由摘要与长度构造。
    #[must_use]
    pub fn new(digest: ContentDigest, bytes: u64) -> Self {
        ExpectedContent { digest, bytes }
    }
}

/// 一次采集请求。
#[derive(Clone, Debug)]
pub struct CaptureRequest {
    /// 工作文件路径（Root 内路径；由适配器按 Root 安全路径语义解释）。
    pub source: std::path::PathBuf,
    /// 对象区 staging 目录下的独占暂存名。
    pub staging_name: String,
    /// 期望内容。
    pub expected: ExpectedContent,
}

/// 对象是否这次新建。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReuseOutcome {
    /// 本次新建并发布。
    FreshlyPublished,
    /// 内容地址上已有同内容对象：未覆盖，复核通过后复用。
    ExistingVerified,
}

impl ReuseOutcome {
    /// 稳定短名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ReuseOutcome::FreshlyPublished => "freshly_published",
            ReuseOutcome::ExistingVerified => "existing_verified",
        }
    }
}

impl fmt::Display for ReuseOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一次成功采集的结果：内容地址、长度、写入 `objects.storage_path` 的相对路径与副本来源。
#[derive(Clone, Debug)]
pub struct CapturedObject {
    /// 内容地址。
    pub digest: ContentDigest,
    /// 对象字节数。
    pub bytes: u64,
    /// 写进 `objects.storage_path` 的规范相对路径。
    pub storage_path: std::path::PathBuf,
    /// 副本来源方式。
    pub method: CopyMethod,
    /// 没走克隆的原因（跨卷或不支持 reflink 时非空）。
    pub fallback: Option<CopyFallback>,
    /// 本次新建还是复用既有对象。
    pub reuse: ReuseOutcome,
    /// 未发布暂存副本的清理结果（复用时清理副本；新建时已由发布消费）。
    pub staging: DiscardOutcome,
}

/// 把工作文件固定成内容地址上的不可变对象。
///
/// # Errors
///
/// 见 [`CaptureFailure`]：端口失败（源不可读、`ENOSPC` 等存储失败、暂存校验失败）、共享
/// inode、暂存别名、长度或摘要不符、既有对象不符、发布后复核失败。失败时不会留下可提交引用。
pub fn capture_object(
    port: &mut dyn ObjectBlobPort,
    layout: &ObjectLayout,
    request: &CaptureRequest,
) -> Result<CapturedObject, CaptureFailure> {
    let staging = layout
        .staging_path(&request.staging_name)
        .map_err(|error| {
            CaptureFailure::new(CaptureFailureReason::from(error), DiscardOutcome::NotNeeded)
        })?;

    // 端口在报错时已删除未验证的暂存文件，因此这里不重复丢弃。
    let staged = port
        .stage_working_copy(&request.source, &staging)
        .map_err(|error| {
            CaptureFailure::new(CaptureFailureReason::Port(error), DiscardOutcome::NotNeeded)
        })?;

    if let Some(reason) = check_staged(&staged, request.expected) {
        return Err(fail_after_staging(port, &staged, reason));
    }

    let object = layout.object_path(&request.expected.digest);
    let published = match port.publish_staged(&staged, &object) {
        Ok(published) => published,
        Err(error) => {
            // 发布失败时暂存副本可能仍在：如实丢弃并报告。
            let staging = discard(port, &staged);
            return Err(CaptureFailure::new(
                CaptureFailureReason::Port(error),
                staging,
            ));
        }
    };

    let facts = match port.verify_object(&object) {
        Ok(facts) => facts,
        Err(error) => {
            let staging = if published.already_present {
                discard(port, &staged)
            } else {
                DiscardOutcome::NotNeeded
            };
            return Err(CaptureFailure::new(
                CaptureFailureReason::Port(error),
                staging,
            ));
        }
    };

    let staging = if published.already_present {
        discard(port, &staged)
    } else {
        DiscardOutcome::NotNeeded
    };

    // 长度先于摘要比较：短写/截断的字节流即使摘要碰巧不同，也要报出更具体的原因。
    if facts.bytes != request.expected.bytes {
        let reason = if published.already_present {
            CaptureFailureReason::ExistingObjectMismatch {
                expected: request.expected.digest,
                observed: facts.digest,
                observed_bytes: facts.bytes,
            }
        } else {
            CaptureFailureReason::PostPublishMismatch {
                expected: request.expected.digest,
                observed: facts.digest,
                observed_bytes: facts.bytes,
            }
        };
        return Err(CaptureFailure::new(reason, staging));
    }
    if facts.digest != request.expected.digest {
        let reason = if published.already_present {
            CaptureFailureReason::ExistingObjectMismatch {
                expected: request.expected.digest,
                observed: facts.digest,
                observed_bytes: facts.bytes,
            }
        } else {
            CaptureFailureReason::PostPublishMismatch {
                expected: request.expected.digest,
                observed: facts.digest,
                observed_bytes: facts.bytes,
            }
        };
        return Err(CaptureFailure::new(reason, staging));
    }

    Ok(CapturedObject {
        digest: request.expected.digest,
        bytes: request.expected.bytes,
        storage_path: ObjectLayout::relative_object_path(&request.expected.digest),
        method: staged.method,
        fallback: staged.fallback,
        reuse: if published.already_present {
            ReuseOutcome::ExistingVerified
        } else {
            ReuseOutcome::FreshlyPublished
        },
        staging,
    })
}

/// 发布前的三项判据：独立 inode、无别名、长度与摘要。
fn check_staged(staged: &StagedBlob, expected: ExpectedContent) -> Option<CaptureFailureReason> {
    if staged.source.same_inode(&staged.staged) {
        return Some(CaptureFailureReason::SharedWritableInode {
            source: staged.source,
            staged: staged.staged,
        });
    }
    if staged.staged.links > 1 {
        return Some(CaptureFailureReason::StagingAlias {
            links: staged.staged.links,
        });
    }
    if staged.staged.bytes != expected.bytes {
        return Some(CaptureFailureReason::LengthMismatch {
            expected: expected.bytes,
            observed: staged.staged.bytes,
        });
    }
    if staged.digest != expected.digest {
        return Some(CaptureFailureReason::DigestMismatch {
            expected: expected.digest,
            observed: staged.digest,
        });
    }
    None
}

/// 丢弃未发布副本；删除失败只记录，不掩盖真正的失败原因。
fn discard(port: &mut dyn ObjectBlobPort, staged: &StagedBlob) -> DiscardOutcome {
    match port.discard_staged(&staged.staging) {
        Ok(()) => DiscardOutcome::Discarded,
        Err(_) => DiscardOutcome::Failed,
    }
}

fn fail_after_staging(
    port: &mut dyn ObjectBlobPort,
    staged: &StagedBlob,
    reason: CaptureFailureReason,
) -> CaptureFailure {
    let staging = discard(port, staged);
    CaptureFailure::new(reason, staging)
}

/// 端口错误里的存储分类（供调用方把 `ENOSPC` 映射成容量告警而不是能力不支持）。
#[must_use]
pub fn storage_failure(error: &BlobError) -> Option<crate::ports::object_store::StorageFailure> {
    match error {
        BlobError::Storage { failure, .. } => Some(*failure),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::error::ReadFailureReason;
    use crate::objects::read::read_object;
    use crate::ports::object_store::{
        FileKey, PublishedBlob, StorageFailure, StoredBytes, StoredFacts, StoredSlice,
    };
    use std::path::{Path, PathBuf};

    const HEX_A: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const HEX_B: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn digest(hex: &str) -> ContentDigest {
        ContentDigest::from_hex(hex).expect("测试用摘要合法")
    }

    fn key(inode: u64, links: u32, bytes: u64) -> FileKey {
        FileKey {
            device: 7,
            inode,
            links,
            bytes,
        }
    }

    /// 记录调用顺序的假端口：stage 脚本决定副本观察结果，verify/read 决定复核结果。
    struct FakePort {
        stage: Option<Result<StagedBlob, BlobError>>,
        verify: Option<Result<StoredFacts, BlobError>>,
        read: Option<Result<StoredBytes, BlobError>>,
        publish_present: bool,
        publish_error: Option<BlobError>,
        discard_error: Option<BlobError>,
        staged_calls: Vec<(PathBuf, PathBuf)>,
        published: Vec<PathBuf>,
        discarded: Vec<PathBuf>,
        verified: Vec<PathBuf>,
    }

    impl FakePort {
        fn new() -> Self {
            FakePort {
                stage: None,
                verify: None,
                read: None,
                publish_present: false,
                publish_error: None,
                discard_error: None,
                staged_calls: Vec::new(),
                published: Vec::new(),
                discarded: Vec::new(),
                verified: Vec::new(),
            }
        }

        fn with_staged(mut self, staged: StagedBlob) -> Self {
            self.stage = Some(Ok(staged));
            self
        }

        fn with_verified(mut self, facts: StoredFacts) -> Self {
            self.verify = Some(Ok(facts));
            self
        }
    }

    impl ObjectBlobPort for FakePort {
        fn stage_working_copy(
            &mut self,
            source: &Path,
            staging: &Path,
        ) -> Result<StagedBlob, BlobError> {
            self.staged_calls
                .push((source.to_path_buf(), staging.to_path_buf()));
            self.stage.take().expect("测试必须预设 stage 结果")
        }

        fn publish_staged(
            &mut self,
            _staged: &StagedBlob,
            object: &Path,
        ) -> Result<PublishedBlob, BlobError> {
            if let Some(error) = self.publish_error.take() {
                return Err(error);
            }
            self.published.push(object.to_path_buf());
            Ok(PublishedBlob {
                object: object.to_path_buf(),
                already_present: self.publish_present,
            })
        }

        fn discard_staged(&mut self, staging: &Path) -> Result<(), BlobError> {
            self.discarded.push(staging.to_path_buf());
            match self.discard_error.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn verify_object(&mut self, object: &Path) -> Result<StoredFacts, BlobError> {
            self.verified.push(object.to_path_buf());
            self.verify.take().expect("测试必须预设 verify 结果")
        }

        fn read_object(&mut self, object: &Path) -> Result<StoredBytes, BlobError> {
            self.verified.push(object.to_path_buf());
            self.read.take().expect("测试必须预设 read 结果")
        }

        fn read_object_range(
            &mut self,
            object: &Path,
            _offset: u64,
            _length: u64,
        ) -> Result<StoredSlice, BlobError> {
            self.verified.push(object.to_path_buf());
            Err(BlobError::InvalidRequest {
                detail: "本用例不读区间".to_owned(),
            })
        }
    }

    fn staged(digest: ContentDigest, bytes: u64) -> StagedBlob {
        StagedBlob {
            staging: PathBuf::from("/obj/staging/cas-1"),
            source: key(10, 1, bytes),
            staged: key(11, 1, bytes),
            digest,
            method: CopyMethod::Reflink,
            fallback: None,
        }
    }

    fn request(digest: ContentDigest, bytes: u64) -> CaptureRequest {
        CaptureRequest {
            source: PathBuf::from("/root/file.bin"),
            staging_name: "cas-1".to_owned(),
            expected: ExpectedContent::new(digest, bytes),
        }
    }

    #[test]
    fn capture_verifies_length_digest_and_publishes_content_address() {
        let wanted = digest(HEX_A);
        let mut port = FakePort::new()
            .with_staged(staged(wanted, 5))
            .with_verified(StoredFacts {
                bytes: 5,
                digest: wanted,
            });
        let layout = ObjectLayout::new("/obj");
        let captured = capture_object(&mut port, &layout, &request(wanted, 5)).expect("采集成功");
        assert_eq!(captured.digest, wanted);
        assert_eq!(captured.bytes, 5);
        assert_eq!(
            captured.storage_path,
            PathBuf::from("objects/11").join(HEX_A)
        );
        assert_eq!(captured.method, CopyMethod::Reflink);
        assert_eq!(captured.reuse, ReuseOutcome::FreshlyPublished);
        assert_eq!(
            port.published,
            vec![PathBuf::from("/obj/objects/11").join(HEX_A)]
        );
        assert_eq!(port.verified.len(), 1, "发布后必须复核一次");
        assert!(port.discarded.is_empty(), "新建成功后没有待清理副本");
    }

    #[test]
    fn digest_mismatch_never_publishes_and_discards_staging() {
        let wanted = digest(HEX_A);
        let mut port = FakePort::new().with_staged(staged(digest(HEX_B), 5));
        let layout = ObjectLayout::new("/obj");
        let failure =
            capture_object(&mut port, &layout, &request(wanted, 5)).expect_err("摘要不符");
        assert_eq!(failure.code(), "digest_mismatch");
        assert!(!failure.is_committable());
        assert_eq!(failure.staging, DiscardOutcome::Discarded);
        assert!(port.published.is_empty(), "摘要不符不得发布");
        assert_eq!(port.discarded, vec![PathBuf::from("/obj/staging/cas-1")]);
    }

    #[test]
    fn short_write_is_a_length_mismatch_and_never_publishes() {
        let wanted = digest(HEX_A);
        let mut port = FakePort::new().with_staged(staged(wanted, 3));
        let layout = ObjectLayout::new("/obj");
        let failure = capture_object(&mut port, &layout, &request(wanted, 5)).expect_err("短写");
        assert_eq!(failure.code(), "length_mismatch");
        assert!(port.published.is_empty());
        assert_eq!(failure.staging, DiscardOutcome::Discarded);
    }

    #[test]
    fn enospc_is_classified_and_never_publishes() {
        let wanted = digest(HEX_A);
        let mut port = FakePort::new();
        port.stage = Some(Err(BlobError::Storage {
            stage: "stage_copy",
            failure: StorageFailure::NoSpace,
            detail: "No space left on device".to_owned(),
        }));
        let layout = ObjectLayout::new("/obj");
        let failure = capture_object(&mut port, &layout, &request(wanted, 5)).expect_err("满盘");
        assert_eq!(failure.code(), "no_space");
        assert_eq!(
            storage_failure(match &failure.reason {
                CaptureFailureReason::Port(error) => error,
                other => panic!("期望端口错误，实际 {other}"),
            }),
            Some(StorageFailure::NoSpace)
        );
        assert!(port.published.is_empty());
        assert!(
            port.discarded.is_empty(),
            "端口在报错时已清理，核心不重复丢弃"
        );
    }

    #[test]
    fn shared_writable_inode_is_refused_before_publish() {
        let wanted = digest(HEX_A);
        let mut aliased = staged(wanted, 5);
        aliased.staged = aliased.source;
        let mut port = FakePort::new().with_staged(aliased);
        let layout = ObjectLayout::new("/obj");
        let failure =
            capture_object(&mut port, &layout, &request(wanted, 5)).expect_err("共享 inode");
        assert_eq!(failure.code(), "shared_writable_inode");
        assert!(port.published.is_empty());
        assert_eq!(failure.staging, DiscardOutcome::Discarded);
    }

    #[test]
    fn hardlinked_staging_is_refused_before_publish() {
        let wanted = digest(HEX_A);
        let mut aliased = staged(wanted, 5);
        aliased.staged = key(11, 2, 5);
        let mut port = FakePort::new().with_staged(aliased);
        let layout = ObjectLayout::new("/obj");
        let failure =
            capture_object(&mut port, &layout, &request(wanted, 5)).expect_err("硬链接别名");
        assert_eq!(failure.code(), "staging_alias");
        assert!(port.published.is_empty());
    }

    #[test]
    fn existing_object_is_reused_only_after_verification() {
        let wanted = digest(HEX_A);
        let mut port = FakePort::new()
            .with_staged(staged(wanted, 5))
            .with_verified(StoredFacts {
                bytes: 5,
                digest: wanted,
            });
        port.publish_present = true;
        let layout = ObjectLayout::new("/obj");
        let captured =
            capture_object(&mut port, &layout, &request(wanted, 5)).expect("复用既有对象");
        assert_eq!(captured.reuse, ReuseOutcome::ExistingVerified);
        assert_eq!(captured.staging, DiscardOutcome::Discarded);

        let mut mismatch = FakePort::new()
            .with_staged(staged(wanted, 5))
            .with_verified(StoredFacts {
                bytes: 5,
                digest: digest(HEX_B),
            });
        mismatch.publish_present = true;
        let failure = capture_object(&mut mismatch, &layout, &request(wanted, 5))
            .expect_err("既有对象内容不符");
        assert_eq!(failure.code(), "existing_object_mismatch");
        assert_eq!(failure.staging, DiscardOutcome::Discarded);
    }

    #[test]
    fn post_publish_mismatch_is_reported_and_staging_cleanup_is_visible() {
        let wanted = digest(HEX_A);
        let mut port = FakePort::new()
            .with_staged(staged(wanted, 5))
            .with_verified(StoredFacts {
                bytes: 4,
                digest: wanted,
            });
        let layout = ObjectLayout::new("/obj");
        let failure =
            capture_object(&mut port, &layout, &request(wanted, 5)).expect_err("发布后不符");
        assert_eq!(failure.code(), "post_publish_mismatch");
        assert!(!failure.is_committable());
        assert_eq!(port.published.len(), 1);

        // 丢弃失败不能让失败原因变成成功，但必须暴露清理结果。
        let mut failing_discard = FakePort::new().with_staged(staged(digest(HEX_B), 5));
        failing_discard.discard_error = Some(BlobError::Storage {
            stage: "discard",
            failure: StorageFailure::Io,
            detail: "EIO".to_owned(),
        });
        let failure = capture_object(&mut failing_discard, &layout, &request(wanted, 5))
            .expect_err("摘要不符");
        assert_eq!(failure.code(), "digest_mismatch");
        assert_eq!(failure.staging, DiscardOutcome::Failed);
    }

    #[test]
    fn invalid_staging_name_fails_before_any_io() {
        let wanted = digest(HEX_A);
        let mut port = FakePort::new();
        let layout = ObjectLayout::new("/obj");
        let mut bad = request(wanted, 5);
        bad.staging_name = "../escape".to_owned();
        let failure = capture_object(&mut port, &layout, &bad).expect_err("非法暂存名");
        assert_eq!(failure.code(), "invalid_request");
        assert_eq!(failure.staging, DiscardOutcome::NotNeeded);
        assert!(port.staged_calls.is_empty());
    }

    #[test]
    fn read_object_verifies_bytes_against_content_address() {
        let wanted = digest(HEX_A);
        let mut port = FakePort::new();
        port.read = Some(Ok(StoredBytes {
            data: b"hello".to_vec(),
            bytes: 5,
            digest: wanted,
        }));
        let layout = ObjectLayout::new("/obj");
        let bytes = read_object(&mut port, &layout, wanted, 5).expect("读取成功");
        assert_eq!(bytes.data, b"hello");

        let mut truncated = FakePort::new();
        truncated.read = Some(Ok(StoredBytes {
            data: b"hel".to_vec(),
            bytes: 3,
            digest: wanted,
        }));
        let failure = read_object(&mut truncated, &layout, wanted, 5).expect_err("截断");
        assert_eq!(failure.code(), "length_mismatch");
        assert!(matches!(
            failure.reason,
            ReadFailureReason::LengthMismatch {
                expected: 5,
                observed: 3
            }
        ));

        let mut rewritten = FakePort::new();
        rewritten.read = Some(Ok(StoredBytes {
            data: b"HELLO".to_vec(),
            bytes: 5,
            digest: digest(HEX_B),
        }));
        let failure = read_object(&mut rewritten, &layout, wanted, 5).expect_err("字节被改写");
        assert_eq!(failure.code(), "digest_mismatch");

        let mut missing = FakePort::new();
        missing.read = Some(Err(BlobError::ObjectMissing {
            detail: "对象路径不存在".to_owned(),
        }));
        let failure = read_object(&mut missing, &layout, wanted, 5).expect_err("对象缺失");
        assert_eq!(failure.code(), "object_missing");
    }
}
