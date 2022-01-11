// - WakerSet -----------------------------------------------------------------

// NOTE based on async-std v1.5.0

use core::{
    cell::UnsafeCell,
    task::{Context, Waker},
};

use heapless::{i, Slab};

// NOTE this should only ever be used in "Thread mode"
pub struct WakerSet {
    inner: UnsafeCell<Inner>,
}

#[allow(unsafe_code)]
impl WakerSet {
    pub const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(Inner::new()),
        }
    }

    pub fn cancel(&self, key: usize) -> bool {
        // NOTE(unsafe) single-threaded context; OK as long as no references are returned
        unsafe { (*self.inner.get()).cancel(key) }
    }

    pub fn notify_any(&self) -> bool {
        // NOTE(unsafe) single-threaded context; OK as long as no references are returned
        unsafe { (*self.inner.get()).notify_any() }
    }

    #[allow(dead_code)]
    pub fn notify_one(&self) -> bool {
        // NOTE(unsafe) single-threaded context; OK as long as no references are returned
        unsafe { (*self.inner.get()).notify_one() }
    }

    pub fn insert(&self, cx: &Context<'_>) -> usize {
        // NOTE(unsafe) single-threaded context; OK as long as no references are returned
        unsafe { (*self.inner.get()).insert(cx) }
    }

    pub fn remove(&self, key: usize) {
        // NOTE(unsafe) single-threaded context; OK as long as no references are returned
        unsafe { (*self.inner.get()).remove(key) }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Notify {
    /// Make sure at least one entry is notified.
    Any,
    /// Notify one additional entry.
    One,
    // Notify all entries.
    // All,
}

struct Inner {
    // NOTE the number of entries is capped at `NTASKS`
    entries: Slab<Option<Waker>, heapless::consts::U8>, // TODO
    notifiable: usize,
}

impl Inner {
    const fn new() -> Self {
        Self {
            entries: Slab(i::Slab::new()),
            notifiable: 0,
        }
    }

    /// Removes the waker of a cancelled operation.
    ///
    /// Returns `true` if another blocked operation from the set was notified.
    fn cancel(&mut self, key: usize) -> bool {
        match self.entries.remove(key) {
            Some(_) => self.notifiable -= 1,
            None => {
                // The operation was cancelled and notified so notify another operation instead.
                for (_, opt_waker) in self.entries.iter_mut() {
                    // If there is no waker in this entry, that means it was already woken.
                    if let Some(w) = opt_waker.take() {
                        w.wake();
                        self.notifiable -= 1;
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Notifies a blocked operation if none have been notified already.
    ///
    /// Returns `true` if an operation was notified.
    fn notify_any(&mut self) -> bool {
        self.notify(Notify::Any)
    }

    /// Notifies one additional blocked operation.
    ///
    /// Returns `true` if an operation was notified.
    fn notify_one(&mut self) -> bool {
        self.notify(Notify::One)
    }

    /// Notifies blocked operations, either one or all of them.
    ///
    /// Returns `true` if at least one operation was notified.
    fn notify(&mut self, n: Notify) -> bool {
        let mut notified = false;

        for (_, opt_waker) in self.entries.iter_mut() {
            // If there is no waker in this entry, that means it was already woken.
            if let Some(w) = opt_waker.take() {
                w.wake();
                self.notifiable -= 1;
                notified = true;

                if n == Notify::One {
                    break;
                }
            }

            if n == Notify::Any {
                break;
            }
        }

        notified
    }

    fn insert(&mut self, cx: &Context<'_>) -> usize {
        let w = cx.waker().clone();
        let key = self.entries.insert(Some(w)).expect("OOM");
        self.notifiable += 1;
        key
    }

    /// Removes the waker of an operation.
    fn remove(&mut self, key: usize) {
        if self.entries.remove(key).is_some() {
            self.notifiable -= 1;
        }
    }
}

// - Mutex --------------------------------------------------------------------

// NOTE waker logic is based on async-std v1.5.0

use core::{
    //cell::{Cell, UnsafeCell},
    cell::Cell,
    future::Future,
    ops,
    pin::Pin,
    //task::{Context, Poll},
    task::Poll,
};

/// A mutual exclusion primitive for protecting shared data
pub struct Mutex<T> {
    locked: Cell<bool>,
    value: UnsafeCell<T>,
    wakers: WakerSet,
}

#[allow(unsafe_code)]
unsafe impl<T> Send for Mutex<T> {}
#[allow(unsafe_code)]
unsafe impl<T> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// Creates a new mutex
    pub const fn new(t: T) -> Self {
        Self {
            locked: Cell::new(false),
            wakers: WakerSet::new(),
            value: UnsafeCell::new(t),
        }
    }

    /// Acquires the lock
    ///
    /// Returns a guard that release the lock when dropped
    pub async fn lock(&self) -> MutexGuard<'_, T> {
        struct Lock<'a, T> {
            mutex: &'a Mutex<T>,
            opt_key: Option<usize>,
        }

        impl<'a, T> Future for Lock<'a, T> {
            type Output = MutexGuard<'a, T>;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                // If the current task is in the set, remove it.
                if let Some(key) = self.opt_key.take() {
                    self.mutex.wakers.remove(key);
                }

                // Try acquiring the lock.
                match self.mutex.try_lock() {
                    Some(guard) => Poll::Ready(guard),
                    None => {
                        // Insert this lock operation.
                        self.opt_key = Some(self.mutex.wakers.insert(cx));

                        Poll::Pending
                    }
                }
            }
        }

        impl<T> Drop for Lock<'_, T> {
            fn drop(&mut self) {
                // If the current task is still in the set, that means it is being cancelled now.
                if let Some(key) = self.opt_key {
                    self.mutex.wakers.cancel(key);
                }
            }
        }

        Lock {
            mutex: self,
            opt_key: None,
        }
        .await
    }

    /// Attempts to acquire the lock
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        if !self.locked.get() {
            self.locked.set(true);
            Some(MutexGuard(self))
        } else {
            None
        }
    }
}

/// A guard that releases the lock when dropped
pub struct MutexGuard<'a, T>(&'a Mutex<T>);

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.0.locked.set(false);
        self.0.wakers.notify_any();
        //asm::wfe();
    }
}

impl<T> ops::Deref for MutexGuard<'_, T> {
    type Target = T;

    #[allow(unsafe_code)]
    fn deref(&self) -> &T {
        unsafe { &*self.0.value.get() }
    }
}

impl<T> ops::DerefMut for MutexGuard<'_, T> {
    #[allow(unsafe_code)]
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.0.value.get() }
    }
}
