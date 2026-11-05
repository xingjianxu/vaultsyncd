//! 账号端口的 SQLite 适配器（`vaultsyncd_server_core::auth::account::OwnerAccounts`）。
//!
//! 只做“端口方法 → `vaultsyncd_server_store::users` 的同一事务调用”的映射：不判密码、不判空状态，
//! 也不提交事务（事务由 [`crate::init`] 持有）。错误按端口语义折叠：登录名冲突单独一类，坏行单独
//! 一类，其余保留原因文本。

use sqlx::{Sqlite, Transaction};
use vaultsyncd_server_core::auth::account::{
    AccountError, NewAccount, OwnerAccounts, StoredAccount,
};
use vaultsyncd_server_store::users::{self, NewUser, UserError};

/// 持有一次写事务的账号适配器。
pub struct SqliteAccounts<'a> {
    /// 由调用方开启并负责提交的写事务。
    pub tx: &'a mut Transaction<'static, Sqlite>,
}

impl OwnerAccounts for SqliteAccounts<'_> {
    async fn count(&mut self) -> Result<u64, AccountError> {
        users::count_users(self.tx).await.map_err(map_error)
    }

    async fn find_by_login(&mut self, login: &str) -> Result<Option<StoredAccount>, AccountError> {
        let record = users::find_by_login(self.tx, login)
            .await
            .map_err(map_error)?;
        Ok(record.map(|record| StoredAccount {
            id: record.id,
            login: record.login,
            password_hash: record.password_hash,
            active: record.status == users::UserStatus::Active,
        }))
    }

    async fn insert(&mut self, new: NewAccount) -> Result<(), AccountError> {
        users::insert_user(
            self.tx,
            &NewUser {
                id: new.id,
                login: new.login,
                password_hash: new.password_hash,
            },
        )
        .await
        .map_err(map_error)
    }
}

fn map_error(error: UserError) -> AccountError {
    match error {
        UserError::DuplicateLogin { login } => AccountError::DuplicateLogin { login },
        UserError::MalformedRow { column } => AccountError::Malformed {
            detail: format!("列 {column} 的值不符合账号表前提"),
        },
        other => AccountError::Storage {
            detail: other.to_string(),
        },
    }
}
