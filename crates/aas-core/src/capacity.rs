//! Upper bound of concurrently alive agent processes. Waiters are served FIFO
//! (tokio's semaphore is fair).

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Clone)]
pub struct Capacity {
    sem: Arc<Semaphore>,
    total: usize,
}

pub type Permit = OwnedSemaphorePermit;

impl Capacity {
    pub fn new(total: usize) -> Self {
        Self {
            sem: Arc::new(Semaphore::new(total)),
            total,
        }
    }

    pub fn try_acquire(&self) -> Option<Permit> {
        self.sem.clone().try_acquire_owned().ok()
    }

    pub async fn acquire(&self) -> Permit {
        self.sem
            .clone()
            .acquire_owned()
            .await
            .expect("capacity semaphore is never closed")
    }

    pub fn in_use(&self) -> usize {
        self.total - self.sem.available_permits()
    }
}
