use crate::permissions::{PermissionResolver, PermissionType};
use axum::async_trait;

pub struct NoPermissions;

#[async_trait]
impl PermissionResolver for NoPermissions {
    async fn has_permission(&self, _username: &str, _permission: PermissionType) -> bool {
        false
    }
}

pub struct AllPermissions;

#[async_trait]
impl PermissionResolver for AllPermissions {
    async fn has_permission(&self, _username: &str, _permission: PermissionType) -> bool {
        true
    }
}
