pub(super) use waker::noop_waker;
pub(super) use yield_future::yield_now;

mod waker {
    /// Returns a [`Waker`] that does nothing when invoked.
    pub(crate) fn noop_waker() -> Waker {
        // SAFETY: see below
        unsafe { Waker::from_raw(noop_raw_waker()) }
    }

    /* SAFETY:
        TODO explain why RawWaker contract is upheld
    */
    use core::task::{RawWaker, RawWakerVTable, Waker};
    const fn noop_raw_waker() -> RawWaker {
        RawWaker::new(core::ptr::null(), &NOOP_WAKER_VTABLE)
    }
    const NOOP_WAKER_VTABLE: RawWakerVTable =
        RawWakerVTable::new(noop_waker_clone, noop, noop, noop);
    unsafe fn noop_waker_clone(_data: *const ()) -> RawWaker {
        noop_raw_waker()
    }
    unsafe fn noop(_data: *const ()) {}
}

mod yield_future {
    /// Returns a [`Future`] that yields control to the executor once before completing.
    pub(crate) fn yield_now() -> YieldFuture {
        YieldFuture(true)
    }

    use core::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };
    pub(crate) struct YieldFuture(bool);
    impl Future for YieldFuture {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            if self.0 {
                self.0 = false;
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }
    }
}
