pub mod logging;
pub mod math;
pub mod time;

use std::sync::{Mutex, MutexGuard};

/// Lock a mutex, recovering from poisoning instead of panicking.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
