//! A pointer type for heap allocation.

use crate::alloc;
use crate::rt;
use std::alloc::Layout;
use std::marker::PhantomData;
use std::pin::Pin;
use std::{borrow, fmt, hash, mem, ptr};

/// A loom version of `std::boxed::Box` based on the leak tracking in
/// [`loom::alloc`](crate::alloc).
///
/// Using this in place of the `std` box lets loom report a box that is never
/// freed, e.g. one turned into a raw pointer with [`Box::into_raw`] and then
/// lost.
///
/// # Differences from `std`
///
/// * `std::boxed::Box` is tightly integrated with the compiler, and a value
///   can be moved out of it by dereferencing (`*boxed`). That cannot be
///   implemented outside of the standard library; use [`Box::into_inner`]
///   instead.
/// * A box of a zero-sized value allocates a byte, which `std` does not, so
///   that it is tracked for leaks like any other: leaking it means that the
///   value's destructor never runs.
/// * A box cannot be coerced to an unsized one: `Box<[T; N]>` to `Box<[T]>`,
///   or `Box<T>` to `Box<dyn Trait>`, needs compiler support no other type
///   can have. Slices and strings have the conversions `std` has besides
///   (from a `Vec`, a slice, an array, an iterator, a `&str` or a `String`).
///   For a trait object, cast the raw pointer, which does the same:
///
///   ```rust
///   # use loom::boxed::Box;
///   # use std::fmt::Debug;
///   # loom::model(|| {
///   let boxed = Box::new(5);
///   let dynamic: Box<dyn Debug> = unsafe { Box::from_raw(Box::into_raw(boxed) as *mut dyn Debug) };
///   # drop(dynamic);
///   # });
///   ```
pub struct Box<T: ?Sized> {
    ptr: ptr::NonNull<T>,
    /// The box owns a `T`, which matters to the drop checker.
    _owns: PhantomData<T>,
}

impl<T> Box<T> {
    /// Allocates memory on the heap and then places `x` into it.
    ///
    /// A zero-sized `T` gets a byte, see the differences from `std` above.
    #[track_caller]
    pub fn new(x: T) -> Self {
        let ptr = allocate(Layout::new::<T>()) as *mut T;

        unsafe { ptr::write(ptr, x) };

        let ptr = unsafe { ptr::NonNull::new_unchecked(ptr) };

        Self {
            ptr,
            _owns: PhantomData,
        }
    }

    /// Constructs a new `Pin<Box<T>>`.
    #[track_caller]
    pub fn pin(x: T) -> Pin<Self> {
        // Safety: the value is on the heap and is never moved out of the box
        // through the pinned pointer.
        unsafe { Pin::new_unchecked(Self::new(x)) }
    }

    /// Consumes the box and returns the value in it.
    ///
    /// A `std` box can be dereferenced like `*std_box` to move the value out;
    /// that relies on compiler support no other type can have, so this
    /// function is the replacement. (`std` has the same function under the
    /// same name, but it is not stable yet.)
    ///
    /// To switch transparently between loom and the standard library, wrap
    /// the difference in a function:
    ///
    /// ```rust
    /// # #[cfg(not(loom))] use std::boxed::Box;
    /// # #[cfg(loom)] use loom::boxed::Box;
    /// fn take<T>(b: Box<T>) -> T {
    ///     #[cfg(not(loom))]
    ///     {
    ///         *b
    ///     }
    ///     #[cfg(loom)]
    ///     {
    ///         Box::into_inner(b)
    ///     }
    /// }
    /// ```
    #[track_caller]
    pub fn into_inner(boxed: Self) -> T {
        let boxed = mem::ManuallyDrop::new(boxed);
        let value = unsafe { ptr::read(boxed.ptr.as_ptr()) };

        // Safety: the value has been moved out, only the memory is left.
        unsafe { boxed.dealloc() };

        value
    }
}

impl<T: ?Sized> Box<T> {
    /// Constructs a box from a raw pointer.
    ///
    /// After calling this function, the raw pointer is owned by the resulting
    /// `Box`. Specifically, the `Box` destructor will call the destructor of
    /// `T` and free the allocated memory.
    ///
    /// # Safety
    ///
    /// The pointer must have come from [`Box::into_raw`], or point to a valid
    /// `T` in memory allocated with [`loom::alloc::alloc`](crate::alloc::alloc)
    /// using the layout of that value (or be any well-aligned non-null
    /// pointer if the value is zero-sized, which is then not tracked for
    /// leaks, as nothing was allocated). Memory from `std`'s `Box` or
    /// allocator is not tracked by loom and must not be passed here. The
    /// pointer must not be used again afterwards; in particular, calling this
    /// twice on the same pointer results in a double free.
    #[inline]
    pub const unsafe fn from_raw(ptr: *mut T) -> Box<T> {
        Self {
            ptr: ptr::NonNull::new_unchecked(ptr),
            _owns: PhantomData,
        }
    }

    /// Consumes the `Box`, returning a wrapped raw pointer.
    ///
    /// The pointer will be properly aligned and non-null.
    ///
    /// After calling this function, the caller is responsible for the memory
    /// previously managed by the `Box`: it has to be turned back into a box
    /// with [`Box::from_raw`] to be released, or loom will report it as
    /// leaked.
    #[inline]
    pub fn into_raw(b: Box<T>) -> *mut T {
        mem::ManuallyDrop::new(b).ptr.as_ptr()
    }

    /// Frees the box's memory without dropping the value in it.
    ///
    /// # Safety
    ///
    /// The value must still be valid enough to compute its layout, and the
    /// box must not be used afterwards.
    #[track_caller]
    unsafe fn dealloc(&self) {
        free(
            self.ptr.as_ptr() as *mut u8,
            Layout::for_value(self.ptr.as_ref()),
        );
    }
}

/// The memory a box of a value with `layout` owns: a zero-sized value gets
/// a byte, as allocating nothing is undefined behavior.
fn storage(layout: Layout) -> Layout {
    if layout.size() == 0 {
        Layout::from_size_align(1, layout.align()).unwrap()
    } else {
        layout
    }
}

/// Allocates the memory for a value with `layout`.
#[track_caller]
fn allocate(layout: Layout) -> *mut u8 {
    let storage = storage(layout);
    let ptr = unsafe { alloc::alloc(storage) };

    if ptr.is_null() {
        std::alloc::handle_alloc_error(storage);
    }

    ptr
}

/// Frees the memory of a box of a value with `layout`.
///
/// # Safety
///
/// `ptr` has to be what the box owns, and not be used afterwards.
#[track_caller]
unsafe fn free(ptr: *mut u8, layout: Layout) {
    // A zero-sized value given to `from_raw` may live at any address, with
    // nothing allocated for it.
    if layout.size() != 0 || rt::is_tracked(ptr) {
        alloc::dealloc(ptr, storage(layout));
    }
}

impl<T> From<Vec<T>> for Box<[T]> {
    /// Moves the elements into a box, as `Vec::into_boxed_slice` does.
    #[track_caller]
    fn from(mut vec: Vec<T>) -> Self {
        let len = vec.len();
        let ptr = allocate(Layout::array::<T>(len).unwrap()) as *mut T;

        unsafe {
            ptr::copy_nonoverlapping(vec.as_ptr(), ptr, len);

            // The elements are in the box now; only the vector's memory is
            // left to free.
            vec.set_len(0);

            Box::from_raw(ptr::slice_from_raw_parts_mut(ptr, len))
        }
    }
}

impl<T: Clone> From<&[T]> for Box<[T]> {
    #[track_caller]
    fn from(slice: &[T]) -> Self {
        Box::from(slice.to_vec())
    }
}

impl<T, const N: usize> From<[T; N]> for Box<[T]> {
    #[track_caller]
    fn from(array: [T; N]) -> Self {
        Box::from(Vec::from(array))
    }
}

impl<T> std::iter::FromIterator<T> for Box<[T]> {
    #[track_caller]
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Box::from(iter.into_iter().collect::<Vec<T>>())
    }
}

impl From<String> for Box<str> {
    #[track_caller]
    fn from(string: String) -> Self {
        let bytes = Box::<[u8]>::from(string.into_bytes());

        // Safety: the bytes are the string's, and so valid UTF-8.
        unsafe { Box::from_raw(Box::into_raw(bytes) as *mut str) }
    }
}

impl From<&str> for Box<str> {
    #[track_caller]
    fn from(string: &str) -> Self {
        Box::from(String::from(string))
    }
}

impl<T: Clone> Clone for Box<[T]> {
    #[track_caller]
    fn clone(&self) -> Self {
        Box::from(&**self)
    }
}

impl Clone for Box<str> {
    #[track_caller]
    fn clone(&self) -> Self {
        Box::from(&**self)
    }
}

impl<T> Default for Box<[T]> {
    #[track_caller]
    fn default() -> Self {
        Box::from(Vec::new())
    }
}

impl Default for Box<str> {
    #[track_caller]
    fn default() -> Self {
        Box::from("")
    }
}

impl<T: ?Sized> Drop for Box<T> {
    #[track_caller]
    fn drop(&mut self) {
        unsafe {
            // The layout has to be read before the value is destroyed.
            let layout = Layout::for_value(self.ptr.as_ref());

            ptr::drop_in_place(self.ptr.as_ptr());

            free(self.ptr.as_ptr() as *mut u8, layout);
        }
    }
}

// Same as `std`: a box is as thread safe as the value it owns.
unsafe impl<T: ?Sized + Send> Send for Box<T> {}
unsafe impl<T: ?Sized + Sync> Sync for Box<T> {}

// Moving the box does not move the value.
impl<T: ?Sized> Unpin for Box<T> {}

impl<T: Default> Default for Box<T> {
    #[track_caller]
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> From<T> for Box<T> {
    #[track_caller]
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: ?Sized> std::ops::Deref for Box<T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: ?Sized> std::ops::DerefMut for Box<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { self.ptr.as_mut() }
    }
}

impl<T: ?Sized> borrow::Borrow<T> for Box<T> {
    fn borrow(&self) -> &T {
        self
    }
}

impl<T: ?Sized> borrow::BorrowMut<T> for Box<T> {
    fn borrow_mut(&mut self) -> &mut T {
        self
    }
}

impl<T: ?Sized> AsRef<T> for Box<T> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T: ?Sized> AsMut<T> for Box<T> {
    fn as_mut(&mut self) -> &mut T {
        self
    }
}

impl<T: fmt::Display + ?Sized> fmt::Display for Box<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

impl<T: fmt::Debug + ?Sized> fmt::Debug for Box<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: Clone> Clone for Box<T> {
    #[inline]
    fn clone(&self) -> Box<T> {
        Self::new(self.as_ref().clone())
    }
}

impl<T: ?Sized + PartialEq> PartialEq for Box<T> {
    #[inline]
    fn eq(&self, other: &Box<T>) -> bool {
        PartialEq::eq(&**self, &**other)
    }

    #[allow(clippy::partialeq_ne_impl)]
    #[inline]
    fn ne(&self, other: &Box<T>) -> bool {
        PartialEq::ne(&**self, &**other)
    }
}

impl<T: ?Sized + Eq> Eq for Box<T> {}

impl<T: ?Sized + PartialOrd> PartialOrd for Box<T> {
    #[inline]
    fn partial_cmp(&self, other: &Box<T>) -> Option<core::cmp::Ordering> {
        PartialOrd::partial_cmp(&**self, &**other)
    }
    #[inline]
    fn lt(&self, other: &Box<T>) -> bool {
        PartialOrd::lt(&**self, &**other)
    }
    #[inline]
    fn le(&self, other: &Box<T>) -> bool {
        PartialOrd::le(&**self, &**other)
    }
    #[inline]
    fn ge(&self, other: &Box<T>) -> bool {
        PartialOrd::ge(&**self, &**other)
    }
    #[inline]
    fn gt(&self, other: &Box<T>) -> bool {
        PartialOrd::gt(&**self, &**other)
    }
}

impl<T: ?Sized + Ord> Ord for Box<T> {
    #[inline]
    fn cmp(&self, other: &Box<T>) -> core::cmp::Ordering {
        Ord::cmp(&**self, &**other)
    }
}

impl<T: ?Sized + hash::Hash> hash::Hash for Box<T> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}
