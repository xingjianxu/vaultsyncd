//! 离线 init 的执行步骤（design §17.1、C27）。
//!
//! 顺序：校验状态目录 → 抢占状态目录锁 → 打开数据库（种类/版本门禁 + 迁移）→ 在**一个写事务**里
//! 走核心用例（空状态、输入形态、Argon2id 散列）→ 写账号 → 提交。任一步失败都不提交，也不留
//! “半个账号”。
//!
//! 与运行中的服务端互斥靠状态目录锁（`vaultsyncd_server_store::state_lock`）：服务端启动时就持有它，
//! 因此 init 拿不到锁时能确定“服务端在运行”，而不是靠读表猜。

use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};

use vaultsyncd_domain::ids::UserId;
use vaultsyncd_server_core::auth::account::OwnerAccounts as _;
use vaultsyncd_server_core::auth::init::{
    InitError, NewOwnerRequest, PreparedOwner, prepare_owner,
};
use vaultsyncd_server_core::auth::password::PasswordHasher;
use vaultsyncd_server_store::db::{Database, DbError};
use vaultsyncd_server_store::state_lock::{StateLock, StateLockError};

use crate::accounts::SqliteAccounts;
use crate::{EXIT_ALREADY_INITIALIZED, EXIT_FAILURE, EXIT_LOCKED, EXIT_USAGE, InitArgs};

/// 服务端状态目录里的数据库文件名（与 `apps/server` 的配置一致）。
const DATABASE_FILE_NAME: &str = "server.sqlite";

/// 初始化成功的事实（不含密码与散列）。
pub struct InitOutcome {
    /// 写入的登录名。
    pub login: String,
    /// 新账号身份。
    pub id: UserId,
    /// 被初始化的数据库路径。
    pub database: PathBuf,
}

/// 初始化失败：带退出码与面向人的原因。
pub struct InitFailure {
    exit_code: u8,
    message: String,
}

impl InitFailure {
    fn new(exit_code: u8, message: String) -> Self {
        Self { exit_code, message }
    }

    /// 进程退出码。
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        self.exit_code
    }

    /// 面向人的原因（不含密码）。
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// 从 stdin 读密码：终端下读两遍且不回显，管道下只读一行。
///
/// 不接受环境变量或参数作为密码来源——那样会进 shell 历史、进程列表或 unit 文件。
///
/// # Errors
///
/// 读写终端/标准输入失败，或两次输入不一致时返回退出码 2 的失败。
pub fn read_password() -> Result<String, InitFailure> {
    let mut stdin = io::stdin();
    if stdin.is_terminal() {
        let first = rpassword::prompt_password("密码：").map_err(|source| {
            InitFailure::new(EXIT_USAGE, format!("从终端读取密码失败：{source}"))
        })?;
        let second = rpassword::prompt_password("再次输入密码：").map_err(|source| {
            InitFailure::new(EXIT_USAGE, format!("从终端读取密码失败：{source}"))
        })?;
        if first != second {
            return Err(InitFailure::new(
                EXIT_USAGE,
                "两次输入的密码不一致".to_owned(),
            ));
        }
        return Ok(first);
    }

    let mut buffer = String::new();
    stdin.read_to_string(&mut buffer).map_err(|source| {
        InitFailure::new(EXIT_USAGE, format!("从标准输入读取密码失败：{source}"))
    })?;
    // 管道输入通常带结尾换行；只去掉一个结尾换行与回车，其余字符原样保留（空格也是密码的一部分）。
    let line = buffer
        .strip_suffix('\n')
        .map(|text| text.strip_suffix('\r').unwrap_or(text))
        .unwrap_or(&buffer);
    Ok(line.to_owned())
}

/// 执行离线初始化。
///
/// # Errors
///
/// 见 [`InitFailure`]：状态目录不可用、锁被占用、数据库打不开、输入不合规、已有账号或写入失败。
pub async fn run(args: &InitArgs, password: &str) -> Result<InitOutcome, InitFailure> {
    let state_dir = &args.state_dir;
    if !state_dir.is_absolute() {
        return Err(InitFailure::new(
            EXIT_USAGE,
            format!("状态目录必须是绝对路径：{}", state_dir.display()),
        ));
    }
    if !state_dir.is_dir() {
        return Err(InitFailure::new(
            EXIT_FAILURE,
            format!("状态目录不存在或不是目录：{}", state_dir.display()),
        ));
    }

    let lock = StateLock::try_acquire_exclusive(state_dir).map_err(|error| match error {
        StateLockError::Busy { .. } => InitFailure::new(
            EXIT_LOCKED,
            format!(
                "状态目录 {} 已被占用：服务端可能正在运行，或另一个 init 正在进行；\
                 init 只在服务端离线时执行",
                state_dir.display()
            ),
        ),
        other => InitFailure::new(EXIT_FAILURE, other.to_string()),
    })?;

    vaultsyncd_server_store::state_lock::ensure_restore_complete(state_dir)
        .map_err(|error| InitFailure::new(EXIT_FAILURE, error.to_string()))?;
    let database_path = state_dir.join(DATABASE_FILE_NAME);
    let database = Database::open(&database_path)
        .await
        .map_err(|error| open_error(&database_path, error))?;

    let outcome = async {
        let mut tx = database
            .begin_write()
            .await
            .map_err(|error| InitFailure::new(EXIT_FAILURE, format!("无法开始写事务：{error}")))?;
        let mut ids = SystemIds;
        let prepared = prepare_owner(
            &mut SqliteAccounts { tx: &mut tx },
            &PasswordHasher::production(),
            &mut ids,
            NewOwnerRequest {
                login: &args.login,
                password,
            },
        )
        .await
        .map_err(init_error)?;
        let record = prepared.new_account();
        SqliteAccounts { tx: &mut tx }
            .insert(record)
            .await
            .map_err(|error| init_error(InitError::Accounts(error)))?;
        tx.commit()
            .await
            .map_err(|error| InitFailure::new(EXIT_FAILURE, format!("提交账号失败：{error}")))?;
        Ok::<PreparedOwner, InitFailure>(prepared)
    }
    .await;

    let prepared = match outcome {
        Ok(prepared) => prepared,
        Err(failure) => {
            // 锁在失败路径上照样要释放：`lock` 在这里被 drop，注释只是让意图显式。
            drop(lock);
            database.close().await;
            return Err(failure);
        }
    };
    database.close().await;

    Ok(InitOutcome {
        login: prepared.login,
        id: prepared.id,
        database: database_path,
    })
}

fn open_error(path: &Path, error: DbError) -> InitFailure {
    InitFailure::new(
        EXIT_FAILURE,
        format!("打开服务端数据库 {} 失败：{error}", path.display()),
    )
}

fn init_error(error: InitError) -> InitFailure {
    let message = error.to_string();
    match error {
        InitError::AlreadyInitialized { .. } => InitFailure::new(EXIT_ALREADY_INITIALIZED, message),
        InitError::InvalidLogin { .. } | InitError::WeakPassword { .. } => {
            InitFailure::new(EXIT_USAGE, message)
        }
        _ => InitFailure::new(EXIT_FAILURE, message),
    }
}

/// 操作系统随机源提供的新身份字节（设计里身份不承担认证，但仍必须是不可预测的随机值）。
struct SystemIds;

impl vaultsyncd_domain::ids::IdSource for SystemIds {
    fn next_id_bytes(&mut self) -> [u8; vaultsyncd_domain::ids::ID_LEN] {
        use rand_core::{OsRng, RngCore as _};
        let mut bytes = [0u8; vaultsyncd_domain::ids::ID_LEN];
        OsRng.fill_bytes(&mut bytes);
        bytes
    }
}
