-- 0003_upload_resumes.sql — 上传续传会话（S055 落地 S054 执行结果）。
--
-- 落 design §9.3「上传的已收块以服务端为准，不能只相信客户端位图」与 §13.2 的上传续传：
-- 客户端只保存两件事——这一条排队操作固定的是哪份本地快照（身份/大小/整份摘要），以及继续
-- 复用的是哪个服务端会话。缺失块仍由 `UploadTarget::status` 从服务端读回，因此这里不存块位图，
-- 也不会与服务端的 `upload_parts` 出现两份互相矛盾的“已收块”事实。
--
-- 不变量：
-- - **一条排队操作最多一个会话**：`op_id` 是主键并外键到 `pending_ops`，操作出队（成功）或
--   被删除时续传记录随之级联清除；不会留下指向已不存在操作的孤儿会话。
-- - **会话只对同一份快照有效**：`snapshot_identity`/`total_size`/`digest` 一起标识快照；
--   任一变化都必须新开会话（S054 `upload::run` 的 `SnapshotChanged` 分支），不能拿旧会话的
--   已收块拼一份新内容。
-- - `digest` 是 SHA-256 原始 32 字节（与 `local_intents.content_hash`、`transfers` 的摘要惯例
--   一致），不是十六进制文本，因此不存在两种编码。
--
-- 0001/0002 已发布，不得改写；本文件只追加新表。
CREATE TABLE upload_resumes (
    op_id             BLOB NOT NULL PRIMARY KEY
                      REFERENCES pending_ops (op_id) ON DELETE CASCADE,
    binding_id        BLOB NOT NULL REFERENCES bindings (id) ON DELETE CASCADE,
    -- 固定快照的本地身份（本地代际或等价的快照标识，只用于续传核对与诊断，不是 wire 身份）。
    snapshot_identity TEXT NOT NULL CHECK (length(snapshot_identity) > 0),
    total_size        INTEGER NOT NULL CHECK (total_size >= 0),
    -- 整份内容的 SHA-256。
    digest            BLOB NOT NULL CHECK (length(digest) = 32),
    -- 服务端上传会话身份（32 位小写十六进制由客户端在写入前校验，本层只要求非空）。
    session_id        TEXT NOT NULL CHECK (length(session_id) > 0)
) STRICT;
