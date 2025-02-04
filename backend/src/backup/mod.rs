use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use color_eyre::eyre::eyre;
use helpers::AtomicFile;
use models::ImageId;
use patch_db::{DbHandle, HasModel};
use reqwest::Url;
use rpc_toolkit::command;
use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tracing::instrument;

use self::target::PackageBackupInfo;
use crate::context::RpcContext;
use crate::dependencies::reconfigure_dependents_with_live_pointers;
use crate::install::PKG_ARCHIVE_DIR;
use crate::net::interface::{InterfaceId, Interfaces};
use crate::net::keys::Key;
use crate::procedure::docker::DockerContainers;
use crate::procedure::{NoOutput, PackageProcedure, ProcedureName};
use crate::s9pk::manifest::PackageId;
use crate::util::serde::{Base32, Base64, IoFormat};
use crate::util::Version;
use crate::version::{Current, VersionT};
use crate::volume::{backup_dir, Volume, VolumeId, Volumes, BACKUP_DIR};
use crate::{Error, ErrorKind, ResultExt};

pub mod backup_bulk;
pub mod os;
pub mod restore;
pub mod target;

#[derive(Debug, Deserialize, Serialize)]
pub struct BackupReport {
    server: ServerBackupReport,
    packages: BTreeMap<PackageId, PackageBackupReport>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ServerBackupReport {
    attempted: bool,
    error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PackageBackupReport {
    error: Option<String>,
}

#[command(subcommands(backup_bulk::backup_all, target::target))]
pub fn backup() -> Result<(), Error> {
    Ok(())
}

#[command(rename = "backup", subcommands(restore::restore_packages_rpc))]
pub fn package_backup() -> Result<(), Error> {
    Ok(())
}

#[derive(Deserialize, Serialize)]
struct BackupMetadata {
    pub timestamp: DateTime<Utc>,
    #[serde(default)]
    pub network_keys: BTreeMap<InterfaceId, Base64<[u8; 32]>>,
    #[serde(default)]
    pub tor_keys: BTreeMap<InterfaceId, Base32<[u8; 64]>>, // DEPRECATED
    pub marketplace_url: Option<Url>,
}

#[derive(Clone, Debug, Deserialize, Serialize, HasModel)]
pub struct BackupActions {
    pub create: PackageProcedure,
    pub restore: PackageProcedure,
}
impl BackupActions {
    pub fn validate(
        &self,
        container: &Option<DockerContainers>,
        eos_version: &Version,
        volumes: &Volumes,
        image_ids: &BTreeSet<ImageId>,
    ) -> Result<(), Error> {
        self.create
            .validate(container, eos_version, volumes, image_ids, false)
            .with_ctx(|_| (crate::ErrorKind::ValidateS9pk, "Backup Create"))?;
        self.restore
            .validate(container, eos_version, volumes, image_ids, false)
            .with_ctx(|_| (crate::ErrorKind::ValidateS9pk, "Backup Restore"))?;
        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn create<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        pkg_id: &PackageId,
        pkg_title: &str,
        pkg_version: &Version,
        interfaces: &Interfaces,
        volumes: &Volumes,
    ) -> Result<PackageBackupInfo, Error> {
        let mut volumes = volumes.to_readonly();
        volumes.insert(VolumeId::Backup, Volume::Backup { readonly: false });
        let backup_dir = backup_dir(pkg_id);
        if tokio::fs::metadata(&backup_dir).await.is_err() {
            tokio::fs::create_dir_all(&backup_dir).await?
        }
        self.create
            .execute::<(), NoOutput>(
                ctx,
                pkg_id,
                pkg_version,
                ProcedureName::CreateBackup,
                &volumes,
                None,
                None,
            )
            .await?
            .map_err(|e| eyre!("{}", e.1))
            .with_kind(crate::ErrorKind::Backup)?;
        let (network_keys, tor_keys) = Key::for_package(&ctx.secret_store, pkg_id)
            .await?
            .into_iter()
            .filter_map(|k| {
                let interface = k.interface().map(|(_, i)| i)?;
                Some((
                    (interface.clone(), Base64(k.as_bytes())),
                    (interface, Base32(k.tor_key().as_bytes())),
                ))
            })
            .unzip();
        let marketplace_url = crate::db::DatabaseModel::new()
            .package_data()
            .idx_model(pkg_id)
            .expect(db)
            .await?
            .installed()
            .expect(db)
            .await?
            .marketplace_url()
            .get(db)
            .await?
            .into_owned();
        let tmp_path = Path::new(BACKUP_DIR)
            .join(pkg_id)
            .join(format!("{}.s9pk", pkg_id));
        let s9pk_path = ctx
            .datadir
            .join(PKG_ARCHIVE_DIR)
            .join(pkg_id)
            .join(pkg_version.as_str())
            .join(format!("{}.s9pk", pkg_id));
        let mut infile = File::open(&s9pk_path).await?;
        let mut outfile = AtomicFile::new(&tmp_path, None::<PathBuf>)
            .await
            .with_kind(ErrorKind::Filesystem)?;
        tokio::io::copy(&mut infile, &mut *outfile)
            .await
            .with_ctx(|_| {
                (
                    crate::ErrorKind::Filesystem,
                    format!("cp {} -> {}", s9pk_path.display(), tmp_path.display()),
                )
            })?;
        outfile.save().await.with_kind(ErrorKind::Filesystem)?;
        let timestamp = Utc::now();
        let metadata_path = Path::new(BACKUP_DIR).join(pkg_id).join("metadata.cbor");
        let mut outfile = AtomicFile::new(&metadata_path, None::<PathBuf>)
            .await
            .with_kind(ErrorKind::Filesystem)?;
        outfile
            .write_all(&IoFormat::Cbor.to_vec(&BackupMetadata {
                timestamp,
                network_keys,
                tor_keys,
                marketplace_url,
            })?)
            .await?;
        outfile.save().await.with_kind(ErrorKind::Filesystem)?;
        Ok(PackageBackupInfo {
            os_version: Current::new().semver().into(),
            title: pkg_title.to_owned(),
            version: pkg_version.clone(),
            timestamp,
        })
    }

    #[instrument(skip_all)]
    pub async fn restore<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        pkg_id: &PackageId,
        pkg_version: &Version,
        interfaces: &Interfaces,
        volumes: &Volumes,
    ) -> Result<(), Error> {
        let mut volumes = volumes.clone();
        volumes.insert(VolumeId::Backup, Volume::Backup { readonly: true });
        self.restore
            .execute::<(), NoOutput>(
                ctx,
                pkg_id,
                pkg_version,
                ProcedureName::RestoreBackup,
                &volumes,
                None,
                None,
            )
            .await?
            .map_err(|e| eyre!("{}", e.1))
            .with_kind(crate::ErrorKind::Restore)?;
        let metadata_path = Path::new(BACKUP_DIR).join(pkg_id).join("metadata.cbor");
        let metadata: BackupMetadata = IoFormat::Cbor.from_slice(
            &tokio::fs::read(&metadata_path).await.with_ctx(|_| {
                (
                    crate::ErrorKind::Filesystem,
                    metadata_path.display().to_string(),
                )
            })?,
        )?;
        let pde = crate::db::DatabaseModel::new()
            .package_data()
            .idx_model(pkg_id)
            .expect(db)
            .await?
            .installed()
            .expect(db)
            .await?;
        pde.marketplace_url()
            .put(db, &metadata.marketplace_url)
            .await?;

        let entry = crate::db::DatabaseModel::new()
            .package_data()
            .idx_model(pkg_id)
            .expect(db)
            .await?
            .installed()
            .expect(db)
            .await?
            .get(db)
            .await?;

        let receipts = crate::config::ConfigReceipts::new(db).await?;
        reconfigure_dependents_with_live_pointers(ctx, db, &receipts, &entry).await?;

        Ok(())
    }
}
