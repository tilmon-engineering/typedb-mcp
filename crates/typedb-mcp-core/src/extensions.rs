//! Per-session typemap for library consumers. See DESIGN.md §3a.
//!
//! Consumer crates that build semantic tools on top of the kernel often
//! need per-session state of their own (current focus entity, accepted
//! disclaimers, a small query cache scoped to the agent's conversation).
//! Each consumer crate stashes its own concrete type — the typemap keys
//! by `TypeId` so two crates can hold independent state without
//! colliding.

use std::{
    any::{Any, TypeId},
    collections::HashMap,
};

/// A heterogeneous typemap. One value per concrete type. Values must be
/// `Send + Sync + 'static` because the kernel may pass `&mut Extensions`
/// across `await` points inside the per-session `Mutex`.
#[derive(Default)]
pub struct Extensions {
    map: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl std::fmt::Debug for Extensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extensions")
            .field("entries", &self.map.len())
            .finish()
    }
}

impl Extensions {
    pub fn new() -> Self { Self::default() }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref::<T>())
    }

    pub fn get_mut<T: Send + Sync + 'static>(&mut self) -> Option<&mut T> {
        self.map
            .get_mut(&TypeId::of::<T>())
            .and_then(|b| b.downcast_mut::<T>())
    }

    /// Insert a value for type `T`, returning the previous value if any.
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) -> Option<T> {
        self.map
            .insert(TypeId::of::<T>(), Box::new(value))
            .and_then(|b| b.downcast::<T>().ok().map(|b| *b))
    }

    /// Get a mutable reference to the value for `T`, inserting the
    /// result of `f` if absent.
    pub fn get_or_insert_with<T, F>(&mut self, f: F) -> &mut T
    where
        T: Send + Sync + 'static,
        F: FnOnce() -> T,
    {
        let id = TypeId::of::<T>();
        self.map
            .entry(id)
            .or_insert_with(|| Box::new(f()))
            .downcast_mut::<T>()
            .expect("typemap invariant: entry for TypeId<T> always holds T")
    }

    /// Remove and return the value for `T`, if present.
    pub fn remove<T: Send + Sync + 'static>(&mut self) -> Option<T> {
        self.map
            .remove(&TypeId::of::<T>())
            .and_then(|b| b.downcast::<T>().ok().map(|b| *b))
    }

    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }

    pub fn len(&self) -> usize { self.map.len() }
    pub fn is_empty(&self) -> bool { self.map.is_empty() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct A(u32);
    #[derive(Debug, PartialEq)]
    struct B(String);

    #[test]
    fn insert_get_remove_roundtrip() {
        let mut ext = Extensions::new();
        assert!(ext.is_empty());
        assert_eq!(ext.insert(A(7)), None);
        assert_eq!(ext.get::<A>(), Some(&A(7)));
        assert_eq!(ext.insert(A(8)), Some(A(7)));
        assert_eq!(ext.remove::<A>(), Some(A(8)));
        assert!(ext.get::<A>().is_none());
    }

    #[test]
    fn distinct_types_dont_collide() {
        let mut ext = Extensions::new();
        ext.insert(A(1));
        ext.insert(B("hi".into()));
        assert_eq!(ext.get::<A>(), Some(&A(1)));
        assert_eq!(ext.get::<B>(), Some(&B("hi".into())));
        assert_eq!(ext.len(), 2);
    }

    #[test]
    fn get_or_insert_with_only_runs_once() {
        let mut ext = Extensions::new();
        let v = ext.get_or_insert_with(|| A(42));
        assert_eq!(*v, A(42));
        let v: &mut A = ext.get_or_insert_with(|| panic!("should not run"));
        assert_eq!(*v, A(42));
    }
}
