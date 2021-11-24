use either::Either;
use std::os::unix::prelude::*;

/// A trait that represents a collection of bytes.
///
/// The role of this trait is similar to [`Buf`] provided by [`bytes`] crate,
/// but it focuses on the situation where all byte chunks are written in a *single*
/// operation.
/// This difference is due to the requirement of FUSE kernel driver that all data in
/// a reply message must be passed in a single `write(2)` syscall.
///
/// [`bytes`]: https://docs.rs/bytes/0.6/bytes
/// [`Buf`]: https://docs.rs/bytes/0.6/bytes/trait.Buf.html
pub trait AtomicBytes {
    /// Return the total amount of bytes contained in this data.
    fn size(&self) -> usize;

    /// Return the number of byte chunks.
    fn count(&self) -> usize;

    /// Fill with potentially multiple slices in this data.
    ///
    /// This method corresonds to [`Buf::bytes_vectored`][bytes_vectored], except that
    /// the number of byte chunks is acquired from `AtomicBytes::count` and the implementation
    /// needs to add all chunks in `dst`.
    ///
    /// [bytes_vectored]: https://docs.rs/bytes/0.6/bytes/trait.Buf.html#method.bytes_vectored
    fn fill_bytes<'a>(&'a self, dst: &mut dyn FillBytes<'a>);
}

/// The container of scattered bytes.
pub trait FillBytes<'a> {
    /// Put a chunk of bytes into this container.
    fn put(&mut self, chunk: &'a [u8]);
}

// ==== pointer types ====

macro_rules! impl_for_pointers {
    ($T:ty) => {
        impl<T: ?Sized> AtomicBytes for $T
        where
            T: AtomicBytes,
        {
            #[inline]
            fn size(&self) -> usize {
                (**self).size()
            }

            #[inline]
            fn count(&self) -> usize {
                (**self).count()
            }

            #[inline]
            fn fill_bytes<'a>(&'a self, dst: &mut dyn FillBytes<'a>) {
                (**self).fill_bytes(dst)
            }
        }
    };
}

impl_for_pointers!(&T);
impl_for_pointers!(&mut T);
impl_for_pointers!(Box<T>);
impl_for_pointers!(std::rc::Rc<T>);
impl_for_pointers!(std::sync::Arc<T>);

// ==== empty bytes ====

impl AtomicBytes for () {
    #[inline]
    fn size(&self) -> usize {
        0
    }

    #[inline]
    fn count(&self) -> usize {
        0
    }

    #[inline]
    fn fill_bytes<'a>(&'a self, _: &mut dyn FillBytes<'a>) {}
}

impl AtomicBytes for [u8; 0] {
    #[inline]
    fn size(&self) -> usize {
        0
    }

    #[inline]
    fn count(&self) -> usize {
        0
    }

    #[inline]
    fn fill_bytes<'a>(&'a self, _: &mut dyn FillBytes<'a>) {}
}

// ==== compound types ====

macro_rules! impl_for_tuple {
    ($($T:ident),+ $(,)?) => {
        #[allow(nonstandard_style)]
        impl<$($T),+> AtomicBytes for ($($T,)+)
        where
            $( $T: AtomicBytes, )+
        {
            #[inline]
            fn size(&self) -> usize {
                let ($($T,)+) = self;
                let mut size = 0;
                $(
                    size += $T.size();
                )+
                size
            }

            #[inline]
            fn count(&self) -> usize {
                let ($($T,)+) = self;
                let mut count = 0;
                $(
                    count += $T.count();
                )+
                count
            }

            #[inline]
            fn fill_bytes<'a>(&'a self, dst: &mut dyn FillBytes<'a>) {
                let ($($T,)+) = self;
                $(
                    AtomicBytes::fill_bytes($T, dst);
                )+
            }
        }
    }
}

impl_for_tuple!(T1);
impl_for_tuple!(T1, T2);
impl_for_tuple!(T1, T2, T3);
impl_for_tuple!(T1, T2, T3, T4);
impl_for_tuple!(T1, T2, T3, T4, T5);

impl<R> AtomicBytes for [R]
where
    R: AtomicBytes,
{
    #[inline]
    fn size(&self) -> usize {
        self.iter().map(|chunk| chunk.size()).sum()
    }

    #[inline]
    fn count(&self) -> usize {
        self.iter().map(|chunk| chunk.count()).sum()
    }

    #[inline]
    fn fill_bytes<'a>(&'a self, dst: &mut dyn FillBytes<'a>) {
        for t in self {
            AtomicBytes::fill_bytes(t, dst);
        }
    }
}

impl<R> AtomicBytes for Vec<R>
where
    R: AtomicBytes,
{
    #[inline]
    fn size(&self) -> usize {
        self.iter().map(|chunk| chunk.size()).sum()
    }

    #[inline]
    fn count(&self) -> usize {
        self.iter().map(|chunk| chunk.count()).sum()
    }

    #[inline]
    fn fill_bytes<'a>(&'a self, dst: &mut dyn FillBytes<'a>) {
        for t in self {
            AtomicBytes::fill_bytes(t, dst);
        }
    }
}

// ==== Option<T> ====

impl<T> AtomicBytes for Option<T>
where
    T: AtomicBytes,
{
    #[inline]
    fn size(&self) -> usize {
        self.as_ref().map_or(0, |b| b.size())
    }

    #[inline]
    fn count(&self) -> usize {
        self.as_ref().map_or(0, |b| b.count())
    }

    #[inline]
    fn fill_bytes<'a>(&'a self, dst: &mut dyn FillBytes<'a>) {
        if let Some(t) = self {
            AtomicBytes::fill_bytes(t, dst)
        }
    }
}

// ==== Either<L, R> ====

impl<L, R> AtomicBytes for Either<L, R>
where
    L: AtomicBytes,
    R: AtomicBytes,
{
    #[inline]
    fn size(&self) -> usize {
        match self {
            Either::Left(l) => l.size(),
            Either::Right(r) => r.size(),
        }
    }

    #[inline]
    fn count(&self) -> usize {
        match self {
            Either::Left(l) => l.count(),
            Either::Right(r) => r.count(),
        }
    }

    #[inline]
    fn fill_bytes<'a>(&'a self, dst: &mut dyn FillBytes<'a>) {
        match self {
            Either::Left(l) => AtomicBytes::fill_bytes(l, dst),
            Either::Right(r) => AtomicBytes::fill_bytes(r, dst),
        }
    }
}

// ==== continuous bytes ====

mod impl_scattered_bytes_for_cont {
    use super::*;

    #[inline(always)]
    fn as_bytes(t: &(impl AsRef<[u8]> + ?Sized)) -> &[u8] {
        t.as_ref()
    }

    macro_rules! impl_for {
        ($($t:ty),*$(,)?) => {$(
            impl AtomicBytes for $t {
                #[inline]
                fn size(&self) -> usize {
                    as_bytes(self).len()
                }

                #[inline]
                fn count(&self) -> usize {
                    if as_bytes(self).is_empty() {
                        0
                    } else {
                        1
                    }
                }

                #[inline]
                fn fill_bytes<'a>(&'a self, dst: &mut dyn FillBytes<'a>) {
                    let this = as_bytes(self);
                    if !this.is_empty() {
                        dst.put(this);
                    }
                }
            }
        )*};
    }

    impl_for! {
        [u8],
        str,
        String,
        Vec<u8>,
        std::borrow::Cow<'_, [u8]>,
    }
}

impl AtomicBytes for std::ffi::OsStr {
    #[inline]
    fn size(&self) -> usize {
        AtomicBytes::size(self.as_bytes())
    }

    #[inline]
    fn count(&self) -> usize {
        AtomicBytes::count(self.as_bytes())
    }

    #[inline]
    fn fill_bytes<'a>(&'a self, dst: &mut dyn FillBytes<'a>) {
        AtomicBytes::fill_bytes(self.as_bytes(), dst)
    }
}

impl AtomicBytes for std::ffi::OsString {
    #[inline]
    fn size(&self) -> usize {
        AtomicBytes::size(self.as_bytes())
    }

    #[inline]
    fn count(&self) -> usize {
        AtomicBytes::count(self.as_bytes())
    }

    #[inline]
    fn fill_bytes<'a>(&'a self, dst: &mut dyn FillBytes<'a>) {
        AtomicBytes::fill_bytes(self.as_bytes(), dst)
    }
}
