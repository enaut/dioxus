use crate::{
    read::Readable, write::Writable, CopyValue, GlobalMemo, GlobalSignal, ReactiveContext,
    ReadableRef,
};
use crate::{Memo, WritableRef};
use dioxus_core::{prelude::IntoAttributeValue, ScopeId};
use generational_box::{AnyStorage, Storage, SyncStorage, UnsyncStorage};
use std::{
    any::Any,
    collections::HashSet,
    ops::{Deref, DerefMut},
    sync::Mutex,
};

/// Creates a new Signal. Signals are a Copy state management solution with automatic dependency tracking.
///
/// ```rust
/// use dioxus::prelude::*;
/// use dioxus_signals::*;
///
/// #[component]
/// fn App() -> Element {
///     let mut count = use_signal(|| 0);
///
///     // Because signals have automatic dependency tracking, if you never read them in a component, that component will not be re-rended when the signal is updated.
///     // The app component will never be rerendered in this example.
///     rsx! { Child { state: count } }
/// }
///
/// #[component]
/// fn Child(mut state: Signal<u32>) -> Element {
///     use_future(move || async move {
///         // Because the signal is a Copy type, we can use it in an async block without cloning it.
///         state += 1;
///     });
///
///     rsx! {
///         button {
///             onclick: move |_| state += 1,
///             "{state}"
///         }
///     }
/// }
/// ```
pub struct Signal<T: 'static, S: Storage<SignalData<T>> = UnsyncStorage> {
    pub(crate) inner: CopyValue<SignalData<T>, S>,
}

/// A signal that can safely shared between threads.
pub type SyncSignal<T> = Signal<T, SyncStorage>;

/// The data stored for tracking in a signal.
pub struct SignalData<T> {
    pub(crate) subscribers: Mutex<HashSet<ReactiveContext>>,
    pub(crate) value: T,
}

impl<T: 'static> Signal<T> {
    /// Creates a new Signal. Signals are a Copy state management solution with automatic dependency tracking.
    #[track_caller]
    pub fn new(value: T) -> Self {
        Self::new_maybe_sync(value)
    }

    /// Create a new signal with a custom owner scope. The signal will be dropped when the owner scope is dropped instead of the current scope.
    #[track_caller]
    pub fn new_in_scope(value: T, owner: ScopeId) -> Self {
        Self::new_maybe_sync_in_scope(value, owner)
    }

    /// Creates a new global Signal that can be used in a global static.
    #[track_caller]
    pub const fn global(constructor: fn() -> T) -> GlobalSignal<T> {
        GlobalSignal::new(constructor)
    }
}

impl<T: PartialEq + 'static> Signal<T> {
    /// Creates a new global Signal that can be used in a global static.
    #[track_caller]
    pub const fn global_memo(constructor: fn() -> T) -> GlobalMemo<T> {
        GlobalMemo::new(constructor)
    }

    /// Creates a new unsync Selector. The selector will be run immediately and whenever any signal it reads changes.
    ///
    /// Selectors can be used to efficiently compute derived data from signals.
    #[track_caller]
    pub fn memo(f: impl FnMut() -> T + 'static) -> Memo<T> {
        Memo::new(f)
    }
}

impl<T: 'static, S: Storage<SignalData<T>>> Signal<T, S> {
    /// Creates a new Signal. Signals are a Copy state management solution with automatic dependency tracking.
    #[track_caller]
    #[tracing::instrument(skip(value))]
    pub fn new_maybe_sync(value: T) -> Self {
        Self {
            inner: CopyValue::<SignalData<T>, S>::new_maybe_sync(SignalData {
                subscribers: Default::default(),
                value,
            }),
        }
    }

    /// Creates a new Signal. Signals are a Copy state management solution with automatic dependency tracking.
    pub fn new_with_caller(
        value: T,
        #[cfg(debug_assertions)] caller: &'static std::panic::Location<'static>,
    ) -> Self {
        Self {
            inner: CopyValue::new_with_caller(
                SignalData {
                    subscribers: Default::default(),
                    value,
                },
                #[cfg(debug_assertions)]
                caller,
            ),
        }
    }

    /// Create a new signal with a custom owner scope. The signal will be dropped when the owner scope is dropped instead of the current scope.
    #[track_caller]
    #[tracing::instrument(skip(value))]
    pub fn new_maybe_sync_in_scope(value: T, owner: ScopeId) -> Self {
        Self {
            inner: CopyValue::<SignalData<T>, S>::new_maybe_sync_in_scope(
                SignalData {
                    subscribers: Default::default(),
                    value,
                },
                owner,
            ),
        }
    }

    /// Drop the value out of the signal, invalidating the signal in the process.
    pub fn manually_drop(&self) -> Option<T> {
        self.inner.manually_drop().map(|i| i.value)
    }

    /// Get the scope the signal was created in.
    pub fn origin_scope(&self) -> ScopeId {
        self.inner.origin_scope()
    }

    fn update_subscribers(&self) {
        {
            let inner = self.inner.read();

            // We cannot hold the subscribers lock while calling mark_dirty, because mark_dirty can run user code which may cause a new subscriber to be added. If we hold the lock, we will deadlock.
            let mut subscribers = std::mem::take(&mut *inner.subscribers.lock().unwrap());
            subscribers.retain(|reactive_context| reactive_context.mark_dirty());
            *inner.subscribers.lock().unwrap() = subscribers;
        }
    }

    /// Get the generational id of the signal.
    pub fn id(&self) -> generational_box::GenerationalBoxId {
        self.inner.id()
    }
}

impl<T, S: Storage<SignalData<T>>> Readable for Signal<T, S> {
    type Target = T;
    type Storage = S;

    #[track_caller]
    fn try_read_unchecked(
        &self,
    ) -> Result<ReadableRef<'static, Self>, generational_box::BorrowError> {
        let inner = self.inner.try_read_unchecked()?;

        if let Some(reactive_context) = ReactiveContext::current() {
            tracing::trace!("Subscribing to the reactive context {}", reactive_context);
            inner.subscribers.lock().unwrap().insert(reactive_context);
        }

        Ok(S::map(inner, |v| &v.value))
    }

    /// Get the current value of the signal. **Unlike read, this will not subscribe the current scope to the signal which can cause parts of your UI to not update.**
    ///
    /// If the signal has been dropped, this will panic.
    #[track_caller]
    fn peek_unchecked(&self) -> ReadableRef<'static, Self> {
        let inner = self.inner.try_read_unchecked().unwrap();
        S::map(inner, |v| &v.value)
    }
}

impl<T: 'static, S: Storage<SignalData<T>>> Writable for Signal<T, S> {
    type Mut<'a, R: ?Sized + 'static> = Write<'a, R, S>;

    fn map_mut<I: ?Sized, U: ?Sized + 'static, F: FnOnce(&mut I) -> &mut U>(
        ref_: Self::Mut<'_, I>,
        f: F,
    ) -> Self::Mut<'_, U> {
        Write::map(ref_, f)
    }

    fn try_map_mut<
        I: ?Sized + 'static,
        U: ?Sized + 'static,
        F: FnOnce(&mut I) -> Option<&mut U>,
    >(
        ref_: Self::Mut<'_, I>,
        f: F,
    ) -> Option<Self::Mut<'_, U>> {
        Write::filter_map(ref_, f)
    }

    fn downcast_lifetime_mut<'a: 'b, 'b, R: ?Sized + 'static>(
        mut_: Self::Mut<'a, R>,
    ) -> Self::Mut<'b, R> {
        Write::downcast_lifetime(mut_)
    }

    #[track_caller]
    fn try_write_unchecked(
        &self,
    ) -> Result<WritableRef<'static, Self>, generational_box::BorrowMutError> {
        self.inner.try_write_unchecked().map(|inner| {
            let borrow = S::map_mut(inner, |v| &mut v.value);
            Write {
                write: borrow,
                drop_signal: Box::new(SignalSubscriberDrop {
                    signal: *self,
                    #[cfg(debug_assertions)]
                    origin: std::panic::Location::caller(),
                }),
            }
        })
    }
}

impl<T> IntoAttributeValue for Signal<T>
where
    T: Clone + IntoAttributeValue,
{
    fn into_value(self) -> dioxus_core::AttributeValue {
        self.with(|f| f.clone().into_value())
    }
}

impl<T: 'static, S: Storage<SignalData<T>>> PartialEq for Signal<T, S> {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

/// Allow calling a signal with signal() syntax
///
/// Currently only limited to copy types, though could probably specialize for string/arc/rc
impl<T: Clone, S: Storage<SignalData<T>> + 'static> Deref for Signal<T, S> {
    type Target = dyn Fn() -> T;

    fn deref(&self) -> &Self::Target {
        Readable::deref_impl(self)
    }
}

#[cfg(feature = "serde")]
impl<T: serde::Serialize + 'static, Store: Storage<SignalData<T>>> serde::Serialize
    for Signal<T, Store>
{
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.read().serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de, T: serde::Deserialize<'de> + 'static, Store: Storage<SignalData<T>>>
    serde::Deserialize<'de> for Signal<T, Store>
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::new_maybe_sync(T::deserialize(deserializer)?))
    }
}

/// A mutable reference to a signal's value.
///
/// T is the current type of the write
/// S is the storage type of the signal
pub struct Write<'a, T: ?Sized + 'static, S: AnyStorage = UnsyncStorage> {
    write: S::Mut<'a, T>,
    drop_signal: Box<dyn Any>,
}

impl<'a, T: ?Sized + 'static, S: AnyStorage> Write<'a, T, S> {
    /// Map the mutable reference to the signal's value to a new type.
    pub fn map<O: ?Sized>(myself: Self, f: impl FnOnce(&mut T) -> &mut O) -> Write<'a, O, S> {
        let Self {
            write, drop_signal, ..
        } = myself;
        Write {
            write: S::map_mut(write, f),
            drop_signal,
        }
    }

    /// Try to map the mutable reference to the signal's value to a new type
    pub fn filter_map<O: ?Sized>(
        myself: Self,
        f: impl FnOnce(&mut T) -> Option<&mut O>,
    ) -> Option<Write<'a, O, S>> {
        let Self {
            write, drop_signal, ..
        } = myself;
        let write = S::try_map_mut(write, f);
        write.map(|write| Write { write, drop_signal })
    }

    /// Downcast the lifetime of the mutable reference to the signal's value.
    ///
    /// This function enforces the variance of the lifetime parameter `'a` in Mut.  Rust will typically infer this cast with a concrete type, but it cannot with a generic type.
    pub fn downcast_lifetime<'b>(mut_: Self) -> Write<'b, T, S>
    where
        'a: 'b,
    {
        Write {
            write: S::downcast_lifetime_mut(mut_.write),
            drop_signal: mut_.drop_signal,
        }
    }
}

impl<T: ?Sized + 'static, S: AnyStorage> Deref for Write<'_, T, S> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.write
    }
}

impl<T: ?Sized, S: AnyStorage> DerefMut for Write<'_, T, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.write
    }
}

struct SignalSubscriberDrop<T: 'static, S: Storage<SignalData<T>>> {
    signal: Signal<T, S>,
    #[cfg(debug_assertions)]
    origin: &'static std::panic::Location<'static>,
}

impl<T: 'static, S: Storage<SignalData<T>>> Drop for SignalSubscriberDrop<T, S> {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        tracing::trace!(
            "Write on signal at {:?} finished, updating subscribers",
            self.origin
        );
        self.signal.update_subscribers();
    }
}
