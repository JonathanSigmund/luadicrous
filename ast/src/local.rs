use crate::{SideEffects, Traverse, Type, TypeSystem, type_system::Infer};
use by_address::ByAddress;
use derive_more::From;
use enum_dispatch::enum_dispatch;
use parking_lot::Mutex;
use std::{
    cell::Cell,
    cmp::Ordering,
    fmt::{self, Display},
    hash::{Hash, Hasher},
};
use triomphe::Arc;

thread_local! {
    static NEXT_LOCAL_ID: Cell<u64> = const { Cell::new(0) };
}

fn next_local_id() -> u64 {
    NEXT_LOCAL_ID.with(|next| {
        let id = next.get();
        next.set(id.checked_add(1).expect("local identity overflow"));
        id
    })
}

pub fn reset_local_ids() {
    NEXT_LOCAL_ID.with(|next| next.set(0));
}

#[derive(Debug, Default, From, Clone, PartialEq, PartialOrd, Ord, Eq, Hash)]
pub struct Local(pub Option<String>);

impl Local {
    pub fn new(name: Option<String>) -> Self {
        Self(name)
    }
}

impl fmt::Display for Local {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.0 {
            Some(name) => write!(f, "{}", name),
            None => write!(f, "UNNAMED_LOCAL"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RcLocal(pub ByAddress<Arc<Mutex<Local>>>, u64);

impl Default for RcLocal {
    fn default() -> Self {
        Self::new(Local::default())
    }
}

impl PartialEq for RcLocal {
    fn eq(&self, other: &Self) -> bool {
        self.1 == other.1
    }
}

impl Eq for RcLocal {}

impl PartialOrd for RcLocal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RcLocal {
    fn cmp(&self, other: &Self) -> Ordering {
        self.1.cmp(&other.1)
    }
}

impl Hash for RcLocal {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.1.hash(state);
    }
}

impl Infer for RcLocal {
    fn infer<'a: 'b, 'b>(&'a mut self, system: &mut TypeSystem<'b>) -> Type {
        system.type_of(self).clone()
    }
}

impl Display for RcLocal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0.0.lock().0 {
            Some(name) => write!(f, "{}", name),
            None => write!(f, "UNNAMED_{}", self.1),
        }
    }
}

impl SideEffects for RcLocal {}

impl Traverse for RcLocal {}

impl RcLocal {
    pub fn new(local: Local) -> Self {
        Self(ByAddress(Arc::new(Mutex::new(local))), next_local_id())
    }
}

#[cfg(test)]
mod tests {
    use super::{RcLocal, reset_local_ids};
    use std::collections::BTreeSet;

    #[test]
    fn local_identity_is_stable_and_unique() {
        reset_local_ids();
        let first = RcLocal::default();
        let first_clone = first.clone();
        let second = RcLocal::default();

        assert_eq!(first, first_clone);
        assert_ne!(first, second);
        assert_eq!(first.to_string(), "UNNAMED_0");
        assert_eq!(second.to_string(), "UNNAMED_1");
        assert_eq!(BTreeSet::from([first, first_clone, second]).len(), 2);
    }
}

impl LocalRw for RcLocal {
    fn values_read(&self) -> Vec<&RcLocal> {
        vec![self]
    }

    fn values_read_mut(&mut self) -> Vec<&mut RcLocal> {
        vec![self]
    }
}

#[enum_dispatch]
pub trait LocalRw {
    fn values_read(&self) -> Vec<&RcLocal> {
        Vec::new()
    }

    fn values_read_mut(&mut self) -> Vec<&mut RcLocal> {
        Vec::new()
    }

    fn values_written(&self) -> Vec<&RcLocal> {
        Vec::new()
    }

    fn values_written_mut(&mut self) -> Vec<&mut RcLocal> {
        Vec::new()
    }

    fn values(&self) -> Vec<&RcLocal> {
        self.values_read()
            .into_iter()
            .chain(self.values_written())
            .collect()
    }

    fn replace_values_read(&mut self, old: &RcLocal, new: &RcLocal) {
        for value in self.values_read_mut() {
            if value == old {
                *value = new.clone();
            }
        }
    }

    fn replace_values_written(&mut self, old: &RcLocal, new: &RcLocal) {
        for value in self.values_written_mut() {
            if value == old {
                *value = new.clone();
            }
        }
    }

    fn replace_values(&mut self, old: &RcLocal, new: &RcLocal) {
        self.replace_values_read(old, new);
        self.replace_values_written(old, new);
    }
}
