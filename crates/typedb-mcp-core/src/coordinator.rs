//! Process-local coordination for destructive database operations.
//! The coordinator is intentionally narrow; normal open/create/delete paths must
//! consult the same instance when migration is enabled.
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReservationError {
    Busy(String),
    InvalidName,
}

#[derive(Clone, Default)]
pub struct OperationCoordinator {
    names: Arc<Mutex<HashSet<String>>>,
}

pub struct NameReservation {
    name: String,
    names: Arc<Mutex<HashSet<String>>>,
}
impl Drop for NameReservation {
    fn drop(&mut self) {
        if let Ok(mut n) = self.names.lock() {
            n.remove(&self.name);
        }
    }
}
impl NameReservation {
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl OperationCoordinator {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn try_reserve(&self, name: &str) -> Result<NameReservation, ReservationError> {
        if !valid_name(name) {
            return Err(ReservationError::InvalidName);
        }
        let mut names = self
            .names
            .lock()
            .map_err(|_| ReservationError::Busy(name.into()))?;
        if !names.insert(name.to_owned()) {
            return Err(ReservationError::Busy(name.into()));
        }
        Ok(NameReservation {
            name: name.to_owned(),
            names: Arc::clone(&self.names),
        })
    }
    pub fn is_reserved(&self, name: &str) -> bool {
        self.names.lock().map(|n| n.contains(name)).unwrap_or(true)
    }
}
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reservation_is_nonblocking_and_raii() {
        let c = OperationCoordinator::new();
        let r = c.try_reserve("x").unwrap();
        assert!(c.try_reserve("x").is_err());
        drop(r);
        assert!(!c.is_reserved("x"));
    }
}
