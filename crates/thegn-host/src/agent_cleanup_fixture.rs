//! Private custody of entries in the real cleanup registries. No resource is
//! created, mounted, synchronized, or torn down by this fixture.
use super::{ProviderSyncRecord, projection_registry, provider_sync_registry};
use thegn_core::projection::ProjectionSpec;

pub(crate) struct RegistryCustody {
    path: String,
    projection: Option<ProjectionSpec>,
    sync: Option<ProviderSyncRecord>,
}
impl RegistryCustody {
    pub(crate) fn install(
        path: &std::path::Path,
        projection: Option<ProjectionSpec>,
        sync: Option<String>,
    ) -> Self {
        let path = path
            .to_str()
            .expect("private UTF-8 fixture path")
            .to_owned();
        assert!(std::path::Path::new(&path).is_absolute());
        let mut projections = projection_registry().lock().unwrap();
        let mut syncs = provider_sync_registry().lock().unwrap();
        let previous_projection = projections.remove(&path);
        let previous_sync = syncs.remove(&path);
        if let Some(projection) = projection {
            projections.insert(path.clone(), projection);
        }
        if let Some(sync) = sync {
            syncs.insert(
                path.clone(),
                ProviderSyncRecord {
                    env_name: sync,
                    in_flight: false,
                    baseline: None,
                },
            );
        }
        Self {
            path,
            projection: previous_projection,
            sync: previous_sync,
        }
    }

    pub(crate) fn snapshot(path: &std::path::Path) -> (Option<String>, Option<String>) {
        let path = path.to_str().unwrap();
        (
            projection_registry()
                .lock()
                .unwrap()
                .get(path)
                .map(|spec| format!("{spec:?}")),
            provider_sync_registry()
                .lock()
                .unwrap()
                .get(path)
                .map(|record| record.env_name.clone()),
        )
    }
}
impl Drop for RegistryCustody {
    fn drop(&mut self) {
        let mut projections = projection_registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut syncs = provider_sync_registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        projections.remove(&self.path);
        syncs.remove(&self.path);
        if let Some(projection) = self.projection.take() {
            projections.insert(self.path.clone(), projection);
        }
        if let Some(sync) = self.sync.take() {
            syncs.insert(self.path.clone(), sync);
        }
    }
}
