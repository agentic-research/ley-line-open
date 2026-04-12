//! Event router (pub/sub bus). Implemented in Task 2.
use std::sync::Arc;

pub struct EventRouter;

impl EventRouter {
    pub fn new(_capacity: usize) -> Arc<Self> {
        Arc::new(EventRouter)
    }
}
