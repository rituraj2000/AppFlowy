use crate::manager::GridUser;
use crate::services::block_meta_editor::GridBlockMetaEditorManager;
use bytes::Bytes;
use flowy_collaboration::client_grid::{GridChangeset, GridMetaPad};
use flowy_collaboration::entities::revision::Revision;
use flowy_collaboration::util::make_delta_from_revisions;
use flowy_error::{FlowyError, FlowyResult};
use flowy_grid_data_model::entities::{
    Cell, CellMetaChangeset, Field, FieldChangeset, FieldMeta, Grid, GridBlockMeta, GridBlockMetaChangeset,
    RepeatedField, RepeatedFieldOrder, RepeatedGridBlock, RepeatedRowOrder, Row, RowMeta, RowMetaChangeset,
};
use std::collections::HashMap;

use crate::dart_notification::{send_dart_notification, GridNotification};
use crate::services::row::{
    make_grid_block_from_block_metas, make_grid_blocks, make_row_ids_per_block, row_meta_from_context,
    serialize_cell_data, GridBlockMetaDataSnapshot, RowMetaContext, RowMetaContextBuilder,
};
use flowy_sync::{RevisionCloudService, RevisionCompactor, RevisionManager, RevisionObjectBuilder};
use lib_infra::future::FutureResult;
use lib_ot::core::PlainTextAttributes;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct ClientGridEditor {
    grid_id: String,
    user: Arc<dyn GridUser>,
    pad: Arc<RwLock<GridMetaPad>>,
    rev_manager: Arc<RevisionManager>,
    block_meta_manager: Arc<GridBlockMetaEditorManager>,
}

impl ClientGridEditor {
    pub async fn new(
        grid_id: &str,
        user: Arc<dyn GridUser>,
        mut rev_manager: RevisionManager,
    ) -> FlowyResult<Arc<Self>> {
        let token = user.token()?;
        let cloud = Arc::new(GridRevisionCloudService { token });
        let grid_pad = rev_manager.load::<GridPadBuilder>(Some(cloud)).await?;
        let rev_manager = Arc::new(rev_manager);
        let pad = Arc::new(RwLock::new(grid_pad));

        let block_meta_manager =
            Arc::new(GridBlockMetaEditorManager::new(grid_id, &user, pad.read().await.get_blocks().clone()).await?);

        Ok(Arc::new(Self {
            grid_id: grid_id.to_owned(),
            user,
            pad,
            rev_manager,
            block_meta_manager,
        }))
    }

    pub async fn create_field(&self, field_meta: FieldMeta) -> FlowyResult<()> {
        let _ = self.modify(|grid| Ok(grid.create_field(field_meta)?)).await?;
        let _ = self.notify_did_update_fields().await?;
        Ok(())
    }

    pub async fn contain_field(&self, field_meta: &FieldMeta) -> bool {
        self.pad.read().await.contain_field(&field_meta.id)
    }

    pub async fn update_field(&self, change: FieldChangeset) -> FlowyResult<()> {
        let _ = self.modify(|grid| Ok(grid.update_field(change)?)).await?;
        Ok(())
    }

    pub async fn delete_field(&self, field_id: &str) -> FlowyResult<()> {
        let _ = self.modify(|grid| Ok(grid.delete_field(field_id)?)).await?;
        Ok(())
    }

    pub async fn create_block(&self, grid_block: GridBlockMeta) -> FlowyResult<()> {
        let _ = self.modify(|grid| Ok(grid.create_block(grid_block)?)).await?;
        Ok(())
    }

    pub async fn update_block(&self, changeset: GridBlockMetaChangeset) -> FlowyResult<()> {
        let _ = self.modify(|grid| Ok(grid.update_block(changeset)?)).await?;
        Ok(())
    }

    pub async fn create_row(&self, start_row_id: Option<String>) -> FlowyResult<()> {
        let field_metas = self.pad.read().await.get_field_metas(None)?;
        let block_id = self.block_id().await?;

        // insert empty row below the row whose id is upper_row_id
        let row_meta_ctx = RowMetaContextBuilder::new(&field_metas).build();
        let row_meta = row_meta_from_context(&block_id, row_meta_ctx);

        // insert the row
        let row_count = self
            .block_meta_manager
            .create_row(&block_id, row_meta, start_row_id)
            .await?;

        // update block row count
        let changeset = GridBlockMetaChangeset::from_row_count(&block_id, row_count);
        let _ = self.update_block(changeset).await?;
        Ok(())
    }

    pub async fn insert_rows(&self, contexts: Vec<RowMetaContext>) -> FlowyResult<()> {
        let block_id = self.block_id().await?;
        let mut rows_by_block_id: HashMap<String, Vec<RowMeta>> = HashMap::new();
        for ctx in contexts {
            let row_meta = row_meta_from_context(&block_id, ctx);
            rows_by_block_id
                .entry(block_id.clone())
                .or_insert_with(Vec::new)
                .push(row_meta);
        }
        let changesets = self.block_meta_manager.insert_row(rows_by_block_id).await?;
        for changeset in changesets {
            let _ = self.update_block(changeset).await?;
        }
        Ok(())
    }

    pub async fn update_row(&self, changeset: RowMetaChangeset) -> FlowyResult<()> {
        self.block_meta_manager.update_row(changeset).await
    }

    pub async fn update_cell(&self, changeset: CellMetaChangeset) -> FlowyResult<()> {
        if let Some(cell_data) = changeset.data.as_ref() {
            match self.pad.read().await.get_field(&changeset.field_id) {
                None => {
                    return Err(FlowyError::internal()
                        .context(format!("Can not find the field with id: {}", &changeset.field_id)));
                }
                Some(field_meta) => {
                    let _ = serialize_cell_data(cell_data, field_meta)?;
                }
            }
        }

        let field_metas = self.get_field_metas(None).await?;
        let row_changeset: RowMetaChangeset = changeset.into();
        let _ = self
            .block_meta_manager
            .update_cells(&field_metas, row_changeset)
            .await?;
        Ok(())
    }

    pub async fn get_grid_blocks(
        &self,
        grid_block_metas: Option<Vec<GridBlockMeta>>,
    ) -> FlowyResult<RepeatedGridBlock> {
        let grid_block_meta_snapshots = self.get_grid_block_meta_snapshots(grid_block_metas.as_ref()).await?;
        let field_meta = self.pad.read().await.get_field_metas(None)?;
        match grid_block_metas {
            None => make_grid_blocks(&field_meta, grid_block_meta_snapshots),
            Some(grid_block_metas) => {
                make_grid_block_from_block_metas(&field_meta, grid_block_metas, grid_block_meta_snapshots)
            }
        }
    }

    pub(crate) async fn get_grid_block_meta_snapshots(
        &self,
        grid_block_infos: Option<&Vec<GridBlockMeta>>,
    ) -> FlowyResult<Vec<GridBlockMetaDataSnapshot>> {
        match grid_block_infos {
            None => {
                let grid_blocks = self.pad.read().await.get_blocks();
                let row_metas_per_block = self
                    .block_meta_manager
                    .get_block_meta_snapshot_from_blocks(grid_blocks)
                    .await?;
                Ok(row_metas_per_block)
            }
            Some(grid_block_infos) => {
                let row_metas_per_block = self
                    .block_meta_manager
                    .get_block_meta_snapshot_from_row_orders(grid_block_infos)
                    .await?;
                Ok(row_metas_per_block)
            }
        }
    }

    pub async fn delete_rows(&self, row_ids: Vec<String>) -> FlowyResult<()> {
        let changesets = self.block_meta_manager.delete_rows(row_ids).await?;
        for changeset in changesets {
            let _ = self.update_block(changeset).await?;
        }
        Ok(())
    }

    pub async fn grid_data(&self) -> FlowyResult<Grid> {
        let field_orders = self.pad.read().await.get_field_orders();
        let block_orders = self.pad.read().await.get_blocks();
        Ok(Grid {
            id: self.grid_id.clone(),
            field_orders,
            blocks: block_orders,
        })
    }

    pub async fn get_field_metas(&self, field_orders: Option<RepeatedFieldOrder>) -> FlowyResult<Vec<FieldMeta>> {
        let field_meta = self.pad.read().await.get_field_metas(field_orders)?;
        Ok(field_meta)
    }

    pub async fn get_blocks(&self) -> FlowyResult<Vec<GridBlockMeta>> {
        let grid_blocks = self.pad.read().await.get_blocks();
        Ok(grid_blocks)
    }

    pub async fn delta_bytes(&self) -> Bytes {
        self.pad.read().await.delta_bytes()
    }

    async fn modify<F>(&self, f: F) -> FlowyResult<()>
    where
        F: for<'a> FnOnce(&'a mut GridMetaPad) -> FlowyResult<Option<GridChangeset>>,
    {
        let mut write_guard = self.pad.write().await;
        match f(&mut *write_guard)? {
            None => {}
            Some(change) => {
                let _ = self.apply_change(change).await?;
            }
        }
        Ok(())
    }

    async fn apply_change(&self, change: GridChangeset) -> FlowyResult<()> {
        let GridChangeset { delta, md5 } = change;
        let user_id = self.user.user_id()?;
        let (base_rev_id, rev_id) = self.rev_manager.next_rev_id_pair();
        let delta_data = delta.to_delta_bytes();
        let revision = Revision::new(
            &self.rev_manager.object_id,
            base_rev_id,
            rev_id,
            delta_data,
            &user_id,
            md5,
        );
        let _ = self
            .rev_manager
            .add_local_revision(&revision, Box::new(GridRevisionCompactor()))
            .await?;
        Ok(())
    }

    async fn block_id(&self) -> FlowyResult<String> {
        match self.pad.read().await.get_blocks().last() {
            None => Err(FlowyError::internal().context("There is no grid block in this grid")),
            Some(grid_block) => Ok(grid_block.block_id.clone()),
        }
    }

    async fn notify_did_update_fields(&self) -> FlowyResult<()> {
        let field_metas = self.get_field_metas(None).await?;
        let repeated_field: RepeatedField = field_metas.into_iter().map(Field::from).collect::<Vec<_>>().into();
        send_dart_notification(&self.grid_id, GridNotification::GridDidUpdateFields)
            .payload(repeated_field)
            .send();
        Ok(())
    }
}

#[cfg(feature = "flowy_unit_test")]
impl ClientGridEditor {
    pub fn rev_manager(&self) -> Arc<RevisionManager> {
        self.rev_manager.clone()
    }
}

pub struct GridPadBuilder();
impl RevisionObjectBuilder for GridPadBuilder {
    type Output = GridMetaPad;

    fn build_object(object_id: &str, revisions: Vec<Revision>) -> FlowyResult<Self::Output> {
        let pad = GridMetaPad::from_revisions(object_id, revisions)?;
        Ok(pad)
    }
}

struct GridRevisionCloudService {
    #[allow(dead_code)]
    token: String,
}

impl RevisionCloudService for GridRevisionCloudService {
    #[tracing::instrument(level = "trace", skip(self))]
    fn fetch_object(&self, _user_id: &str, _object_id: &str) -> FutureResult<Vec<Revision>, FlowyError> {
        FutureResult::new(async move { Ok(vec![]) })
    }
}

struct GridRevisionCompactor();
impl RevisionCompactor for GridRevisionCompactor {
    fn bytes_from_revisions(&self, revisions: Vec<Revision>) -> FlowyResult<Bytes> {
        let delta = make_delta_from_revisions::<PlainTextAttributes>(revisions)?;
        Ok(delta.to_delta_bytes())
    }
}
