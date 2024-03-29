use std::sync::{Arc, Mutex};

use crate::permissions::{PermissionResolver, PermissionType};
use axum::async_trait;
use octocrab::models::UserId;

pub struct NoPermissions;

#[async_trait]
impl PermissionResolver for NoPermissions {
    async fn has_permission(&self, _username: &UserId, _permission: PermissionType) -> bool {
        false
    }
    async fn reload(&self) {}
}

pub struct AllPermissions;

#[async_trait]
impl PermissionResolver for AllPermissions {
    async fn has_permission(&self, _username: &UserId, _permission: PermissionType) -> bool {
        true
    }
    async fn reload(&self) {}
}

pub struct MockPermissions {
    pub num_reload_called: i32,
}

impl Default for MockPermissions {
    fn default() -> Self {
        Self {
            num_reload_called: 0,
        }
    }
}

#[async_trait]
impl PermissionResolver for Arc<Mutex<MockPermissions>> {
    async fn has_permission(&self, _username: &UserId, _permission: PermissionType) -> bool {
        false
    }
    async fn reload(&self) {
        self.lock().unwrap().num_reload_called += 1
    }
}
