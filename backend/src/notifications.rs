use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use color_eyre::eyre::eyre;
use patch_db::{DbHandle, LockType};
use rpc_toolkit::command;
use sqlx::PgPool;
use tokio::sync::Mutex;
use tracing::instrument;

use crate::backup::BackupReport;
use crate::context::RpcContext;
use crate::s9pk::manifest::PackageId;
use crate::util::display_none;
use crate::util::serde::display_serializable;
use crate::{Error, ErrorKind, ResultExt};

#[command(subcommands(list, delete, delete_before, create))]
pub async fn notification() -> Result<(), Error> {
    Ok(())
}

#[command(display(display_serializable))]
#[instrument(skip_all)]
pub async fn list(
    #[context] ctx: RpcContext,
    #[arg] before: Option<i32>,
    #[arg] limit: Option<u32>,
) -> Result<Vec<Notification>, Error> {
    let limit = limit.unwrap_or(40);
    let mut handle = ctx.db.handle();
    match before {
        None => {
            let model = crate::db::DatabaseModel::new()
                .server_info()
                .unread_notification_count();
            model.lock(&mut handle, LockType::Write).await?;
            let records = sqlx::query!(
                "SELECT id, package_id, created_at, code, level, title, message, data FROM notifications ORDER BY id DESC LIMIT $1",
                limit as i64
            ).fetch_all(&ctx.secret_store).await?;
            let notifs = records
                .into_iter()
                .map(|r| {
                    Ok(Notification {
                        id: r.id as u32,
                        package_id: r.package_id.and_then(|p| p.parse().ok()),
                        created_at: DateTime::from_utc(r.created_at, Utc),
                        code: r.code as u32,
                        level: match r.level.parse::<NotificationLevel>() {
                            Ok(a) => a,
                            Err(e) => return Err(e.into()),
                        },
                        title: r.title,
                        message: r.message,
                        data: match r.data {
                            None => serde_json::Value::Null,
                            Some(v) => match v.parse::<serde_json::Value>() {
                                Ok(a) => a,
                                Err(e) => {
                                    return Err(Error::new(
                                        eyre!("Invalid Notification Data: {}", e),
                                        ErrorKind::ParseDbField,
                                    ))
                                }
                            },
                        },
                    })
                })
                .collect::<Result<Vec<Notification>, Error>>()?;
            // set notification count to zero
            model.put(&mut handle, &0).await?;
            Ok(notifs)
        }
        Some(before) => {
            let records = sqlx::query!(
                "SELECT id, package_id, created_at, code, level, title, message, data FROM notifications WHERE id < $1 ORDER BY id DESC LIMIT $2",
                before,
                limit as i64
            ).fetch_all(&ctx.secret_store).await?;
            let res = records
                .into_iter()
                .map(|r| {
                    Ok(Notification {
                        id: r.id as u32,
                        package_id: r.package_id.and_then(|p| p.parse().ok()),
                        created_at: DateTime::from_utc(r.created_at, Utc),
                        code: r.code as u32,
                        level: match r.level.parse::<NotificationLevel>() {
                            Ok(a) => a,
                            Err(e) => return Err(e.into()),
                        },
                        title: r.title,
                        message: r.message,
                        data: match r.data {
                            None => serde_json::Value::Null,
                            Some(v) => match v.parse::<serde_json::Value>() {
                                Ok(a) => a,
                                Err(e) => {
                                    return Err(Error::new(
                                        eyre!("Invalid Notification Data: {}", e),
                                        ErrorKind::ParseDbField,
                                    ))
                                }
                            },
                        },
                    })
                })
                .collect::<Result<Vec<Notification>, Error>>()?;
            Ok(res)
        }
    }
}

#[command(display(display_none))]
pub async fn delete(#[context] ctx: RpcContext, #[arg] id: i32) -> Result<(), Error> {
    sqlx::query!("DELETE FROM notifications WHERE id = $1", id)
        .execute(&ctx.secret_store)
        .await?;
    Ok(())
}

#[command(rename = "delete-before", display(display_none))]
pub async fn delete_before(#[context] ctx: RpcContext, #[arg] before: i32) -> Result<(), Error> {
    sqlx::query!("DELETE FROM notifications WHERE id < $1", before)
        .execute(&ctx.secret_store)
        .await?;
    Ok(())
}

#[command(display(display_none))]
pub async fn create(
    #[context] ctx: RpcContext,
    #[arg] package: Option<PackageId>,
    #[arg] level: NotificationLevel,
    #[arg] title: String,
    #[arg] message: String,
) -> Result<(), Error> {
    ctx.notification_manager
        .notify(
            &mut ctx.db.handle(),
            package,
            level,
            title,
            message,
            (),
            None,
        )
        .await
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationLevel {
    Success,
    Info,
    Warning,
    Error,
}
impl fmt::Display for NotificationLevel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            NotificationLevel::Success => write!(f, "success"),
            NotificationLevel::Info => write!(f, "info"),
            NotificationLevel::Warning => write!(f, "warning"),
            NotificationLevel::Error => write!(f, "error"),
        }
    }
}
pub struct InvalidNotificationLevel(String);
impl From<InvalidNotificationLevel> for crate::Error {
    fn from(val: InvalidNotificationLevel) -> Self {
        Error::new(
            eyre!("Invalid Notification Level: {}", val.0),
            ErrorKind::ParseDbField,
        )
    }
}
impl FromStr for NotificationLevel {
    type Err = InvalidNotificationLevel;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            s if s == "success" => Ok(NotificationLevel::Success),
            s if s == "info" => Ok(NotificationLevel::Info),
            s if s == "warning" => Ok(NotificationLevel::Warning),
            s if s == "error" => Ok(NotificationLevel::Error),
            s => Err(InvalidNotificationLevel(s.to_string())),
        }
    }
}
impl fmt::Display for InvalidNotificationLevel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Invalid Notification Level: {}", self.0)
    }
}
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Notification {
    id: u32,
    package_id: Option<PackageId>, // TODO change for package id newtype
    created_at: DateTime<Utc>,
    code: u32,
    level: NotificationLevel,
    title: String,
    message: String,
    data: serde_json::Value,
}

pub trait NotificationType:
    serde::Serialize + for<'de> serde::Deserialize<'de> + std::fmt::Debug
{
    const CODE: i32;
}

impl NotificationType for () {
    const CODE: i32 = 0;
}
impl NotificationType for BackupReport {
    const CODE: i32 = 1;
}

pub struct NotificationManager {
    sqlite: PgPool,
    cache: Mutex<HashMap<(Option<PackageId>, NotificationLevel, String), i64>>,
}
impl NotificationManager {
    pub fn new(sqlite: PgPool) -> Self {
        NotificationManager {
            sqlite,
            cache: Mutex::new(HashMap::new()),
        }
    }
    #[instrument(skip_all)]
    pub async fn notify<Db: DbHandle, T: NotificationType>(
        &self,
        db: &mut Db,
        package_id: Option<PackageId>,
        level: NotificationLevel,
        title: String,
        message: String,
        subtype: T,
        debounce_interval: Option<u32>,
    ) -> Result<(), Error> {
        if !self
            .should_notify(&package_id, &level, &title, debounce_interval)
            .await
        {
            return Ok(());
        }
        let mut count = crate::db::DatabaseModel::new()
            .server_info()
            .unread_notification_count()
            .get_mut(db)
            .await?;
        let sql_package_id = package_id.as_ref().map(|p| &**p);
        let sql_code = T::CODE;
        let sql_level = format!("{}", level);
        let sql_data =
            serde_json::to_string(&subtype).with_kind(crate::ErrorKind::Serialization)?;
        sqlx::query!(
        "INSERT INTO notifications (package_id, code, level, title, message, data) VALUES ($1, $2, $3, $4, $5, $6)",
        sql_package_id,
        sql_code as i32,
        sql_level,
        title,
        message,
        sql_data
    ).execute(&self.sqlite).await?;
        *count += 1;
        count.save(db).await?;
        Ok(())
    }
    async fn should_notify(
        &self,
        package_id: &Option<PackageId>,
        level: &NotificationLevel,
        title: &String,
        debounce_interval: Option<u32>,
    ) -> bool {
        let mut guard = self.cache.lock().await;
        let k = (package_id.clone(), level.clone(), title.clone());
        let v = (*guard).get(&k);
        match v {
            None => {
                (*guard).insert(k, Utc::now().timestamp());
                true
            }
            Some(last_issued) => match debounce_interval {
                None => {
                    (*guard).insert(k, Utc::now().timestamp());
                    true
                }
                Some(interval) => {
                    if last_issued + interval as i64 > Utc::now().timestamp() {
                        false
                    } else {
                        (*guard).insert(k, Utc::now().timestamp());
                        true
                    }
                }
            },
        }
    }
}

#[test]
fn serialization() {
    println!(
        "{}",
        serde_json::json!({ "test": "abcdefg", "num": 32, "nested": { "inner": null, "xyz": [0,2,4]}})
    )
}
