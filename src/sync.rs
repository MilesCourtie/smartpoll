#[cfg(not(loom))]
mod without_loom {
    extern crate alloc;
    pub(crate) use alloc::sync::Arc;

    pub(crate) use core::sync::atomic;

    pub(crate) mod cell {
        pub(crate) struct UnsafeCell<T>(core::cell::UnsafeCell<T>);
        impl<T> UnsafeCell<T> {
            pub(crate) fn new(data: T) -> Self {
                Self(core::cell::UnsafeCell::new(data))
            }
            pub(crate) fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
                f(self.0.get())
            }
        }
    }
}

#[cfg(not(loom))]
pub(crate) use without_loom::*;

#[cfg(loom)]
mod with_loom {
    extern crate alloc;
    pub(crate) use alloc::sync::Arc as StdArc;

    pub(crate) use loom::sync::{atomic, Arc};

    pub(crate) mod cell {
        pub(crate) use loom::cell::UnsafeCell;
    }
}

#[cfg(loom)]
pub(crate) use with_loom::*;
