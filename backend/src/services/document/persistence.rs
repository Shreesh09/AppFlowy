use crate::{
    context::FlowyPersistence,
    services::kv::{KVStore, KeyValue},
    util::serde_ext::parse_from_bytes,
};
use anyhow::Context;
use backend_service::errors::{internal_error, ServerError};
use bytes::Bytes;
use flowy_collaboration::protobuf::{
    CreateDocParams,
    DocIdentifier,
    DocumentInfo,
    RepeatedRevision,
    ResetDocumentParams,
    Revision,
};
use lib_ot::{core::OperationTransformable, rich_text::RichTextDelta};
use protobuf::Message;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

#[tracing::instrument(level = "debug", skip(kv_store), err)]
pub(crate) async fn create_document(
    kv_store: &Arc<DocumentKVPersistence>,
    mut params: CreateDocParams,
) -> Result<(), ServerError> {
    let revisions = params.take_revisions().take_items();
    let _ = kv_store.batch_set_revision(revisions.into()).await?;
    Ok(())
}

#[tracing::instrument(level = "debug", skip(persistence), err)]
pub(crate) async fn read_document(
    persistence: &Arc<FlowyPersistence>,
    params: DocIdentifier,
) -> Result<DocumentInfo, ServerError> {
    let _ = Uuid::parse_str(&params.doc_id).context("Parse document id to uuid failed")?;

    let kv_store = persistence.kv_store();
    let revisions = kv_store.batch_get_revisions(&params.doc_id, None).await?;
    make_doc_from_revisions(&params.doc_id, revisions)
}

#[tracing::instrument(level = "debug", skip(kv_store, params), fields(delta), err)]
pub async fn reset_document(
    kv_store: &Arc<DocumentKVPersistence>,
    params: ResetDocumentParams,
) -> Result<(), ServerError> {
    // TODO: Reset document requires atomic operation
    // let _ = kv_store.batch_delete_revisions(&doc_id.to_string(), None).await?;
    todo!()
}

#[tracing::instrument(level = "debug", skip(kv_store), err)]
pub(crate) async fn delete_document(kv_store: &Arc<DocumentKVPersistence>, doc_id: Uuid) -> Result<(), ServerError> {
    let _ = kv_store.batch_delete_revisions(&doc_id.to_string(), None).await?;
    Ok(())
}

pub struct DocumentKVPersistence {
    inner: Arc<dyn KVStore>,
}

impl std::ops::Deref for DocumentKVPersistence {
    type Target = Arc<dyn KVStore>;

    fn deref(&self) -> &Self::Target { &self.inner }
}

impl std::ops::DerefMut for DocumentKVPersistence {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.inner }
}

impl DocumentKVPersistence {
    pub(crate) fn new(kv_store: Arc<dyn KVStore>) -> Self { DocumentKVPersistence { inner: kv_store } }

    pub(crate) async fn batch_set_revision(&self, revisions: Vec<Revision>) -> Result<(), ServerError> {
        let kv_store = self.inner.clone();
        let items = revisions
            .into_iter()
            .map(|revision| {
                let key = make_revision_key(&revision.doc_id, revision.rev_id);
                let value = Bytes::from(revision.write_to_bytes().unwrap());
                KeyValue { key, value }
            })
            .collect::<Vec<KeyValue>>();
        let _ = kv_store.batch_set(items).await?;
        // use futures::stream::{self, StreamExt};
        // let f = |revision: Revision, kv_store: Arc<dyn KVStore>| async move {
        //     let key = make_revision_key(&revision.doc_id, revision.rev_id);
        //     let bytes = revision.write_to_bytes().unwrap();
        //     let _ = kv_store.set(&key, Bytes::from(bytes)).await.unwrap();
        // };
        //
        // stream::iter(revisions)
        //     .for_each_concurrent(None, |revision| f(revision, kv_store.clone()))
        //     .await;
        Ok(())
    }

    pub(crate) async fn get_doc_revisions(&self, doc_id: &str) -> Result<RepeatedRevision, ServerError> {
        let items = self.inner.batch_get_start_with(doc_id).await?;
        Ok(key_value_items_to_revisions(items))
    }

    pub(crate) async fn batch_get_revisions<T: Into<Option<Vec<i64>>>>(
        &self,
        doc_id: &str,
        rev_ids: T,
    ) -> Result<RepeatedRevision, ServerError> {
        let rev_ids = rev_ids.into();
        let items = match rev_ids {
            None => self.inner.batch_get_start_with(doc_id).await?,
            Some(rev_ids) => {
                let keys = rev_ids
                    .into_iter()
                    .map(|rev_id| make_revision_key(doc_id, rev_id))
                    .collect::<Vec<String>>();
                self.inner.batch_get(keys).await?
            },
        };

        Ok(key_value_items_to_revisions(items))
    }

    pub(crate) async fn batch_delete_revisions<T: Into<Option<Vec<i64>>>>(
        &self,
        doc_id: &str,
        rev_ids: T,
    ) -> Result<(), ServerError> {
        match rev_ids.into() {
            None => {
                let _ = self.inner.batch_delete_key_start_with(doc_id).await?;
                Ok(())
            },
            Some(rev_ids) => {
                let keys = rev_ids
                    .into_iter()
                    .map(|rev_id| make_revision_key(doc_id, rev_id))
                    .collect::<Vec<String>>();
                let _ = self.inner.batch_delete(keys).await?;
                Ok(())
            },
        }
    }
}

#[inline]
fn key_value_items_to_revisions(items: Vec<KeyValue>) -> RepeatedRevision {
    let mut revisions = items
        .into_iter()
        .filter_map(|kv| parse_from_bytes::<Revision>(&kv.value).ok())
        .collect::<Vec<Revision>>();

    revisions.sort_by(|a, b| a.rev_id.cmp(&b.rev_id));
    let mut repeated_revision = RepeatedRevision::new();
    repeated_revision.set_items(revisions.into());
    repeated_revision
}

#[inline]
fn make_revision_key(doc_id: &str, rev_id: i64) -> String { format!("{}:{}", doc_id, rev_id) }

#[inline]
fn make_doc_from_revisions(doc_id: &str, mut revisions: RepeatedRevision) -> Result<DocumentInfo, ServerError> {
    let revisions = revisions.take_items();
    if revisions.is_empty() {
        return Err(ServerError::record_not_found().context(format!("{} not exist", doc_id)));
    }

    let mut document_delta = RichTextDelta::new();
    let mut base_rev_id = 0;
    let mut rev_id = 0;
    // TODO: generate delta from revision should be wrapped into function.
    for revision in revisions {
        base_rev_id = revision.base_rev_id;
        rev_id = revision.rev_id;
        let delta = RichTextDelta::from_bytes(revision.delta_data).map_err(internal_error)?;
        document_delta = document_delta.compose(&delta).map_err(internal_error)?;
    }
    let text = document_delta.to_json();
    let mut document_info = DocumentInfo::new();
    document_info.set_doc_id(doc_id.to_owned());
    document_info.set_text(text);
    document_info.set_base_rev_id(base_rev_id);
    document_info.set_rev_id(rev_id);
    Ok(document_info)
}