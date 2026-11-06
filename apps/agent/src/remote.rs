//! Production remote ports; root and device are fixed by the durable binding.
use std::sync::Arc;
use vaultsyncd_agent_core::{
    bindings::import::{ImportSnapshot, SnapshotSeed},
    engine::events::ChangeSource,
    ports::remote::RemotePort,
};
use vaultsyncd_domain::{
    ids::{BindingId, OpId, RootId},
    version::{RootSeq, UtcTimestamp},
};
use vaultsyncd_http_client::{
    ApiClient, ReqwestTransport, changes, content::ContentEndpoint, sync, uploads::UploadEndpoint,
};
use vaultsyncd_protocol::generated::{ChangeList, OperationRequest, OperationResult, SnapshotPage};
use vaultsyncd_transfer::ports::{PortError, PortFuture, UploadTarget, VersionedContent};
/// One binding's authenticated HTTP ports; rejects cross-binding routing.
pub struct Remote {
    pub(crate) client: ApiClient<ReqwestTransport>,
    pub(crate) binding: BindingId,
    pub(crate) root: RootId,
}
impl Remote {
    fn check(&self, binding: BindingId) -> Result<(), PortError> {
        if self.binding != binding {
            return Err(PortError::protocol("binding route mismatch"));
        }
        Ok(())
    }
}
impl RemotePort for Remote {
    fn submit(
        &self,
        request: &OperationRequest,
    ) -> PortFuture<'_, Result<OperationResult, PortError>> {
        let request = request.clone();
        Box::pin(async move {
            sync::submit(&self.client, &self.root.to_string(), &request)
                .await
                .map_err(vaultsyncd_http_client::transfer_error)
        })
    }
    fn operation(
        &self,
        binding: BindingId,
        op: OpId,
    ) -> PortFuture<'_, Result<OperationResult, PortError>> {
        Box::pin(async move {
            self.check(binding)?;
            sync::operation(&self.client, &self.root.to_string(), &op.to_string())
                .await
                .map_err(vaultsyncd_http_client::transfer_error)
        })
    }
    fn upload(
        &self,
        binding: BindingId,
    ) -> PortFuture<'_, Result<Arc<dyn UploadTarget>, PortError>> {
        Box::pin(async move {
            self.check(binding)?;
            Ok(Arc::new(
                UploadEndpoint::new(self.client.clone(), self.root.to_string())
                    .map_err(vaultsyncd_http_client::transfer_error)?,
            ) as Arc<dyn UploadTarget>)
        })
    }
    fn content(
        &self,
        binding: BindingId,
    ) -> PortFuture<'_, Result<Arc<dyn VersionedContent>, PortError>> {
        Box::pin(async move {
            self.check(binding)?;
            Ok(Arc::new(
                ContentEndpoint::new(self.client.clone(), self.root.to_string())
                    .map_err(vaultsyncd_http_client::transfer_error)?,
            ) as Arc<dyn VersionedContent>)
        })
    }
}
impl ImportSnapshot for Remote {
    fn begin(&self, root: RootId) -> PortFuture<'_, Result<SnapshotSeed, PortError>> {
        Box::pin(async move {
            if root != self.root {
                return Err(PortError::protocol("root mismatch"));
            }
            let (metadata, time) = sync::snapshot(&self.client, &root.to_string())
                .await
                .map_err(vaultsyncd_http_client::transfer_error)?;
            Ok(SnapshotSeed {
                metadata,
                server_time: UtcTimestamp::from_unix_seconds(
                    i64::try_from(time).map_err(|e| PortError::protocol(e.to_string()))?,
                )
                .map_err(|e| PortError::protocol(e.to_string()))?,
            })
        })
    }
    fn page<'a>(
        &'a self,
        root: RootId,
        snapshot: &'a str,
        cursor: Option<&'a str>,
    ) -> PortFuture<'a, Result<SnapshotPage, PortError>> {
        Box::pin(async move {
            if root != self.root {
                return Err(PortError::protocol("root mismatch"));
            }
            sync::snapshot_page(&self.client, &root.to_string(), snapshot, cursor)
                .await
                .map_err(vaultsyncd_http_client::transfer_error)
        })
    }
}
impl ChangeSource for Remote {
    fn pull(
        &self,
        root: RootId,
        after: RootSeq,
        limit: u32,
    ) -> PortFuture<'_, Result<ChangeList, PortError>> {
        Box::pin(async move {
            if root != self.root {
                return Err(PortError::protocol("root mismatch"));
            }
            changes::list(
                &self.client,
                &root.to_string(),
                &after.to_string(),
                Some(limit),
            )
            .await
            .map_err(vaultsyncd_http_client::transfer_error)
        })
    }
}
