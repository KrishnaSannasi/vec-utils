#![feature(try_trait)]

use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ops::Try;

trait VecExt: Sized {
    type T;

    fn map<U, F: FnMut(Self::T) -> U>(self, mut f: F) -> Vec<U> {
        use std::convert::Infallible;

        match self.try_map(move |x| Ok::<_, Infallible>(f(x))) {
            Ok(x) => x,
            Err(x) => match x {},
        }
    }

    fn try_map<U, R: Try<Ok = U>, F: FnMut(Self::T) -> R>(self, f: F) -> Result<Vec<U>, R::Error>;

    fn zip_with<U, V, F: FnMut(Self::T, U) -> V>(self, other: Vec<U>, mut f: F) -> Vec<V> {
        use std::convert::Infallible;

        match self.try_zip_with(other, move |x, y| Ok::<_, Infallible>(f(x, y))) {
            Ok(x) => x,
            Err(x) => match x {},
        }
    }

    fn try_zip_with<U, V, R: Try<Ok = V>, F: FnMut(Self::T, U) -> R>(
        self,
        other: Vec<U>,
        f: F,
    ) -> Result<Vec<V>, R::Error>;

    fn drop_and_reuse<U>(self) -> Vec<U>;
}

impl<T> VecExt for Vec<T> {
    type T = T;

    fn try_map<U, R: Try<Ok = U>, F: FnMut(Self::T) -> R>(self, f: F) -> Result<Vec<U>, R::Error> {
        use std::alloc::Layout;

        if Layout::new::<T>() == Layout::new::<U>() {
            let iter = MapIter {
                init_len: 0,
                data: VecData::from(self),
                drop: PhantomData,
            };

            iter.try_into_vec(f)
        } else {
            self.into_iter().map(f).map(R::into_result).collect()
        }
    }

    fn try_zip_with<U, V, R: Try<Ok = V>, F: FnMut(Self::T, U) -> R>(
        self,
        other: Vec<U>,
        mut f: F,
    ) -> Result<Vec<V>, R::Error> {
        use std::alloc::Layout;

        match (
            Layout::new::<T>() == Layout::new::<V>(),
            Layout::new::<U>() == Layout::new::<V>(),
            self.capacity() >= other.capacity(),
        ) {
            (true, true, true) | (true, false, _) => ZipWithIter {
                init_len: 0,
                min_len: self.len().min(other.len()),
                drop: PhantomData,

                left: VecData::from(self),
                right: VecData::from(other),
            }
            .try_into_vec(f),
            (true, true, false) | (false, true, _) => ZipWithIter {
                init_len: 0,
                min_len: self.len().min(other.len()),
                drop: PhantomData,

                left: VecData::from(other),
                right: VecData::from(self),
            }
            .try_into_vec(move |x, y| f(y, x)),
            (false, false, _) => self
                .into_iter()
                .zip(other.into_iter())
                .map(move |(x, y)| f(x, y))
                .map(R::into_result)
                .collect(),
        }
    }

    fn drop_and_reuse<U>(mut self) -> Vec<U> {
        self.clear();

        self.map(|_| unsafe { std::hint::unreachable_unchecked() })
    }
}

/// This allows running destructors, even if other destructors have panicked
macro_rules! defer {
    ($($do_work:tt)*) => {
        let _guard = OnDrop(Some(|| { $($do_work)* }));
    }
}

struct OnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        self.0.take().unwrap()()
    }
}

struct VecData<T> {
    // the start of the vec data segment
    start: *mut T,

    // the current position of the vec data segment
    ptr: *mut T,

    // the length of the vec data segment
    len: usize,

    // the capacity of the vec data segment
    cap: usize,

    drop: PhantomData<T>,
}

impl<T> From<Vec<T>> for VecData<T> {
    fn from(vec: Vec<T>) -> Self {
        let mut vec = ManuallyDrop::new(vec);
        let ptr = vec.as_mut_ptr();

        Self {
            start: ptr,
            ptr,
            len: vec.len(),
            cap: vec.capacity(),
            drop: PhantomData,
        }
    }
}

struct MapIter<T, U> {
    init_len: usize,

    data: VecData<T>,

    // for drop check
    drop: PhantomData<U>,
}

impl<T, U> MapIter<T, U> {
    fn try_into_vec<R: Try<Ok = U>, F: FnMut(T) -> R>(
        mut self,
        mut f: F,
    ) -> Result<Vec<U>, R::Error> {
        // does a pointer walk, easy for LLVM to optimize
        while self.init_len < self.data.len {
            unsafe {
                let value = f(self.data.ptr.read())?;

                (self.data.ptr as *mut U).write(value);

                self.data.ptr = self.data.ptr.add(1);
                self.init_len += 1;
            }
        }

        let vec = ManuallyDrop::new(self);

        // we don't want to free the memory
        // which is what dropping this `MapIter` will do
        unsafe {
            Ok(Vec::from_raw_parts(
                vec.data.start as *mut U,
                vec.data.len,
                vec.data.cap,
            ))
        }
    }
}

impl<T, U> Drop for MapIter<T, U> {
    fn drop(&mut self) {
        unsafe {
            // destroy the initialized output
            defer! {
                Vec::from_raw_parts(
                    self.data.start as *mut U,
                    self.init_len,
                    self.data.cap
                );
            }

            // offset by 1 because self.ptr is pointing to
            // memory that was just read from, dropping that
            // would lead to a double free
            std::ptr::drop_in_place(std::slice::from_raw_parts_mut(
                self.data.ptr.add(1),
                self.data.len - self.init_len - 1,
            ));
        }
    }
}

// The size of these structures don't matter since they are transient
// So I didn't bother optimizing the size of them, and instead put all the
// useful information I wanted, so that it could be initialized all at once
struct ZipWithIter<T, U, V> {
    // This left buffer is the one that will be reused
    // to write the output into
    left: VecData<T>,

    // We will only read from this buffer
    //
    // I considered using `std::vec::IntoIter`, but that lead to worse code
    // because LLVM wasn't able to elide the bounds check on the iterator
    right: VecData<U>,

    // the length of the output that has been written to
    init_len: usize,
    // the length of the vectors that must be traversed
    min_len: usize,

    // for drop check
    drop: PhantomData<V>,
}

impl<T, U, V> ZipWithIter<T, U, V> {
    fn try_into_vec<R: Try<Ok = V>, F: FnMut(T, U) -> R>(
        mut self,
        mut f: F,
    ) -> Result<Vec<V>, R::Error> {
        use std::alloc::Layout;

        debug_assert_eq!(Layout::new::<T>(), Layout::new::<V>());

        // this does a pointer walk and reads from left and right in lock-step
        // then passes those values to the function to be processed
        while self.init_len < self.min_len {
            unsafe {
                let value = f(self.left.ptr.read(), self.right.ptr.read())?;

                (self.left.ptr as *mut V).write(value);

                self.left.ptr = self.left.ptr.add(1);
                self.right.ptr = self.right.ptr.add(1);

                self.init_len += 1;
            }
        }

        // We don't want to drop `self` if dropping the excess elements panics
        // as that could lead to double drops
        let vec = ManuallyDrop::new(self);
        let output;

        unsafe {
            // create the vector now, so that if we panic in drop, we don't leak it
            output = Vec::from_raw_parts(vec.left.start as *mut V, vec.min_len, vec.left.cap);

            // yay for defers running in reverse order and cleaning up the
            // old vecs properly

            // cleans up the right vec
            defer! {
                Vec::from_raw_parts(vec.right.start, 0, vec.right.cap);
            }

            // drops the remaining elements of the right vec
            defer! {
                std::ptr::drop_in_place(std::slice::from_raw_parts_mut(
                    vec.right.ptr,
                    vec.right.len - vec.min_len
                ));
            }

            // drop the remaining elements of the left vec
            std::ptr::drop_in_place(std::slice::from_raw_parts_mut(
                vec.left.ptr,
                vec.left.len - vec.min_len,
            ));
        }

        Ok(output)
    }
}

impl<T, U, V> Drop for ZipWithIter<T, U, V> {
    fn drop(&mut self) {
        unsafe {
            // This will happen last
            //
            // frees the allocated memory, but does not run destructors
            defer! {
                Vec::from_raw_parts(self.left.start, 0, self.left.cap);
                Vec::from_raw_parts(self.right.start, 0, self.right.cap);
            }

            // The order of the next two defers don't matter for correctness
            //
            // They free the remaining parts of the two input vectors
            defer! {
                std::ptr::drop_in_place(std::slice::from_raw_parts_mut(self.right.ptr.add(1), self.right.len - self.init_len - 1));
            }

            defer! {
                std::ptr::drop_in_place(std::slice::from_raw_parts_mut(self.left.ptr.add(1), self.left.len - self.init_len - 1));
            }

            // drop the output that we already calculated
            std::ptr::drop_in_place(std::slice::from_raw_parts_mut(
                self.left.start as *mut V,
                self.init_len,
            ));
        }
    }
}
