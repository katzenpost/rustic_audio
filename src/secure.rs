//! Memory for audio that must not leak, such as a voice note before it is sent: between two
//! inaccessible guard pages, after a canary checked when it is freed, locked out of swap where the
//! system allows, left out of core dumps, and wiped whenever it is freed or outgrown. Should
//! guarded memory be refused, the samples live on the heap instead, and are still wiped.

use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

use zeroize::Zeroize;

pub struct SecureVec<T: Copy + Zeroize> {
    store: Store<T>,
    len: usize,
}

enum Store<T> {
    Guarded { ptr: NonNull<T>, cap: usize },
    Heap(Vec<T>),
}

// SAFETY: the guarded memory is owned by the SecureVec alone.
unsafe impl<T: Copy + Zeroize + Send> Send for SecureVec<T> {}
unsafe impl<T: Copy + Zeroize + Sync> Sync for SecureVec<T> {}

/// Guarded memory for `cap` values of `T`, or nothing.
fn alloc<T>(cap: usize) -> Option<NonNull<T>> {
    let bytes = cap.checked_mul(std::mem::size_of::<T>())?;
    // SAFETY: malloc_sized hands back `bytes` writable bytes of its own, or nothing. They end on a
    // page boundary, so they start `bytes` before one: aligned for T, as `bytes` is a multiple of
    // T's size, which is a multiple of its alignment.
    let p = unsafe { memsec::malloc_sized(bytes)? };
    let p = p.cast::<T>();
    debug_assert_eq!(p.as_ptr() as usize % std::mem::align_of::<T>(), 0);
    leave_out_of_dumps(p.as_ptr().cast(), bytes);
    Some(p)
}

/// memsec does this on Linux, but not on Android.
#[cfg(target_os = "android")]
fn leave_out_of_dumps(p: *mut u8, bytes: usize) {
    // SAFETY: sysconf and madvise only read their arguments; the pages are ours.
    unsafe {
        let page = libc::sysconf(libc::_SC_PAGESIZE).max(4096) as usize;
        let start = (p as usize - 16) & !(page - 1);
        let end = (p as usize + bytes).div_ceil(page) * page;
        libc::madvise(start as *mut libc::c_void, end - start, libc::MADV_DONTDUMP);
    }
}

#[cfg(not(target_os = "android"))]
fn leave_out_of_dumps(_: *mut u8, _: usize) {}

/// Wipe and give back guarded memory from [`alloc`].
fn free<T>(ptr: NonNull<T>, cap: usize) {
    if cap == 0 {
        return;
    }
    let bytes = cap * std::mem::size_of::<T>();
    // SAFETY: allocated by memsec::malloc_sized with `bytes` bytes; free wipes it, and aborts if its
    // canary was overwritten.
    unsafe { memsec::free(NonNull::slice_from_raw_parts(ptr.cast::<u8>(), bytes)) }
}

impl<T: Copy + Zeroize> SecureVec<T> {
    pub fn new() -> Self {
        Self {
            store: Store::Guarded {
                ptr: NonNull::dangling(),
                cap: 0,
            },
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the values are in guarded memory.
    pub fn is_guarded(&self) -> bool {
        matches!(self.store, Store::Guarded { .. })
    }

    fn capacity(&self) -> usize {
        match &self.store {
            Store::Guarded { cap, .. } => *cap,
            Store::Heap(v) => v.capacity(),
        }
    }

    /// Room for `more` values: a larger allocation, the values moved there, the old one wiped.
    pub fn reserve(&mut self, more: usize) {
        let need = self.len.checked_add(more).expect("SecureVec too large");
        if need <= self.capacity() {
            return;
        }
        let cap = need
            .max(self.capacity() * 2)
            .max(4096 / std::mem::size_of::<T>().max(1));
        let len = self.len;
        match &mut self.store {
            Store::Guarded { ptr, cap: old } => match alloc::<T>(cap) {
                Some(p) => {
                    // SAFETY: both hold at least `len` values and do not overlap.
                    unsafe { std::ptr::copy_nonoverlapping(ptr.as_ptr(), p.as_ptr(), len) };
                    free(*ptr, *old);
                    *ptr = p;
                    *old = cap;
                }
                None => {
                    let mut v = Vec::with_capacity(cap);
                    // SAFETY: `len` values are initialised.
                    v.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr.as_ptr(), len) });
                    free(*ptr, *old);
                    self.store = Store::Heap(v);
                }
            },
            Store::Heap(v) => {
                let mut n = Vec::with_capacity(cap);
                n.extend_from_slice(v);
                v.zeroize();
                *v = n;
            }
        }
    }

    pub fn push(&mut self, x: T) {
        self.extend_from_slice(std::slice::from_ref(&x));
    }

    pub fn extend_from_slice(&mut self, xs: &[T]) {
        self.reserve(xs.len());
        match &mut self.store {
            // SAFETY: reserve made room for `xs` after the `len` values.
            Store::Guarded { ptr, .. } => unsafe {
                std::ptr::copy_nonoverlapping(xs.as_ptr(), ptr.as_ptr().add(self.len), xs.len());
            },
            Store::Heap(v) => v.extend_from_slice(xs),
        }
        self.len += xs.len();
    }

    /// A copy on the heap, wiped when dropped, for handing on to code that needs a `Vec`.
    pub fn to_zeroizing_vec(&self) -> zeroize::Zeroizing<Vec<T>> {
        zeroize::Zeroizing::new(self.to_vec())
    }
}

impl<T: Copy + Zeroize> Default for SecureVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy + Zeroize> Deref for SecureVec<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        match &self.store {
            // SAFETY: `len` values are initialised, and the memory is ours until drop.
            Store::Guarded { ptr, .. } => unsafe { std::slice::from_raw_parts(ptr.as_ptr(), self.len) },
            Store::Heap(v) => v,
        }
    }
}

impl<T: Copy + Zeroize> DerefMut for SecureVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        match &mut self.store {
            // SAFETY: as for deref, and borrowed mutably.
            Store::Guarded { ptr, .. } => unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr(), self.len) },
            Store::Heap(v) => v,
        }
    }
}

impl<T: Copy + Zeroize> Drop for SecureVec<T> {
    fn drop(&mut self) {
        match &mut self.store {
            Store::Guarded { ptr, cap } => free(*ptr, *cap),
            Store::Heap(v) => v.zeroize(),
        }
    }
}

impl std::io::Write for SecureVec<u8> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::SecureVec;

    #[test]
    fn grows_and_keeps_its_values() {
        let mut v = SecureVec::<f32>::new();
        for i in 0..100_000 {
            v.push(i as f32);
        }
        assert!(v.is_guarded());
        assert_eq!(v.len(), 100_000);
        assert!(v.iter().enumerate().all(|(i, &x)| x == i as f32));
        v[7] = -1.0;
        assert_eq!(v[7], -1.0);
    }

    #[test]
    fn writes_bytes() {
        use std::io::Write;
        let mut v = SecureVec::<u8>::new();
        v.write_all(b"OggS").unwrap();
        v.extend_from_slice(&[0; 5000]);
        assert_eq!(&v[..4], b"OggS");
        assert_eq!(v.len(), 5004);
        assert_eq!(*v.to_zeroizing_vec(), v.to_vec());
    }

    #[test]
    fn empty() {
        let v = SecureVec::<u8>::new();
        assert!(v.is_empty());
        assert_eq!(&*v, &[] as &[u8]);
    }
}
