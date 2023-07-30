#[cfg(loom)]
pub(crate) use with_loom::*;
#[cfg(not(loom))]
pub(crate) use without_loom::*;

#[cfg(not(loom))]
mod without_loom {
    extern crate alloc;
    pub(crate) use alloc::sync::Arc;
    pub(crate) use core::sync::atomic;

    /// A Wrapper around [`core::cell::UnsafeCell`] which provides the same API as
    /// [`loom::cell::UnsafeCell`].
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

#[cfg(loom)]
mod with_loom {
    pub(crate) use loom::cell::UnsafeCell;
    pub(crate) use loom::sync::{atomic, Arc};
}
