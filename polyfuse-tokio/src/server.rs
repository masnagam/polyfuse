//! Serve FUSE filesystem.

use crate::channel::Channel;
use futures::{
    future::{Future, FutureExt},
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    select,
    task::{self, Poll},
};
use libc::c_int;
use pin_project::pin_project;
use polyfuse::{Filesystem, Session, SessionInitializer};
use std::{
    ffi::OsStr,
    io::{self, IoSlice, IoSliceMut},
    path::Path,
    pin::Pin,
    sync::Arc,
};
use tokio::signal::unix::{signal, SignalKind};

/// A FUSE filesystem server running on Tokio runtime.
#[derive(Debug)]
pub struct Server {
    session: Arc<Session>,
    channel: Channel,
}

impl Server {
    /// Create a FUSE server mounted on the specified path.
    pub async fn mount(mountpoint: impl AsRef<Path>, mountopts: &[&OsStr]) -> io::Result<Self> {
        let mut channel = Channel::open(mountpoint.as_ref(), mountopts)?;

        let initializer = SessionInitializer::default();
        let mut buf = vec![0u8; initializer.init_buf_size()];
        let session = loop {
            channel.read(&mut buf[..]).await?;
            match initializer
                .try_init(&mut unite(&mut &buf[..], &mut channel))
                .await?
            {
                Some(session) => break session,
                None => continue,
            }
        };

        let channel = channel;

        Ok(Server {
            session: Arc::new(session),
            channel,
        })
    }

    /// Attempt to make a clone of this instance.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            session: self.session.clone(),
            channel: self.channel.try_clone()?,
        })
    }

    /// Run a FUSE filesystem daemon.
    pub async fn run<F>(&mut self, fs: F) -> io::Result<()>
    where
        F: Filesystem + Send + 'static,
    {
        let sig = default_shutdown_signal()?;
        let _sig = self.run_until(fs, sig).await?;
        Ok(())
    }

    /// Run a FUSE filesystem until the specified signal is received.
    #[allow(clippy::unnecessary_mut_passed)]
    pub async fn run_until<F, S>(&mut self, fs: F, sig: S) -> io::Result<Option<S::Output>>
    where
        F: Filesystem + Send + 'static,
        S: Future + Unpin,
    {
        let Self {
            session,
            ref mut channel,
        } = self;
        let fs = Arc::new(fs);
        let mut sig = sig.fuse();

        let mut main_loop = Box::pin(async move {
            loop {
                let mut buf = vec![0u8; session.buffer_size()];
                if let Err(err) = channel.read(&mut buf[..]).await {
                    match err.raw_os_error() {
                        Some(libc::ENODEV) => {
                            tracing::debug!("connection is closed");
                            return Ok(());
                        }
                        _ => return Err(err),
                    }
                }

                let session = session.clone();
                let fs = fs.clone();
                let mut writer = channel.try_clone()?;
                tokio::spawn(async move {
                    if let Err(e) = session
                        .process(&*fs, &mut unite(&mut &buf[..], &mut writer))
                        .await
                    {
                        tracing::error!("error during handling a request: {}", e);
                    }
                });
            }
        })
        .fuse();

        select! {
            _ = main_loop => Ok(None),
            sig = sig => Ok(Some(sig)),
        }
    }

    /// Invalidate the specified range of cache data for an inode.
    ///
    /// When the kernel receives this notification, some requests are queued to read
    /// the updated data.
    pub async fn notify_inval_inode(&mut self, ino: u64, off: i64, len: i64) -> io::Result<()> {
        self.session
            .notify_inval_inode(&mut self.channel, ino, off, len)
            .await
    }

    /// Invalidate an entry with the specified name in the directory.
    pub async fn notify_inval_entry(
        &mut self,
        parent: u64,
        name: impl AsRef<OsStr>,
    ) -> io::Result<()> {
        self.session
            .notify_inval_entry(&mut self.channel, parent, name)
            .await
    }

    /// Notify that an entry with the specified name has been deleted from the directory.
    pub async fn notify_delete(
        &mut self,
        parent: u64,
        child: u64,
        name: impl AsRef<OsStr>,
    ) -> io::Result<()> {
        self.session
            .notify_delete(&mut self.channel, parent, child, name)
            .await
    }

    /// Replace the specified range of cache data with a new value.
    pub async fn notify_store(&mut self, ino: u64, offset: u64, data: &[&[u8]]) -> io::Result<()> {
        self.session
            .notify_store(&mut self.channel, ino, offset, data)
            .await
    }

    /// Retrieve the value of the cache data with the specified range.
    pub async fn notify_retrieve(&mut self, ino: u64, offset: u64, size: u32) -> io::Result<u64> {
        self.session
            .notify_retrieve(&mut self.channel, ino, offset, size)
            .await
    }

    /// Notify an I/O readiness.
    pub async fn notify_poll_wakeup(&mut self, kh: u64) -> io::Result<()> {
        self.session.notify_poll_wakeup(&mut self.channel, kh).await
    }
}

#[allow(clippy::unnecessary_mut_passed)]
fn default_shutdown_signal() -> io::Result<impl Future<Output = c_int> + Unpin> {
    let mut sighup = signal(SignalKind::hangup())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigpipe = signal(SignalKind::pipe())?;

    Ok(Box::pin(async move {
        // TODO: use stabilized API.
        let mut sighup = Box::pin(sighup.recv()).fuse();
        let mut sigint = Box::pin(sigint.recv()).fuse();
        let mut sigterm = Box::pin(sigterm.recv()).fuse();
        let mut sigpipe = Box::pin(sigpipe.recv()).fuse();

        loop {
            select! {
                _ = sighup => {
                    tracing::debug!("Got SIGHUP");
                    return libc::SIGHUP;
                },
                _ = sigint => {
                    tracing::debug!("Got SIGINT");
                    return libc::SIGINT;
                },
                _ = sigterm => {
                    tracing::debug!("Got SIGTERM");
                    return libc::SIGTERM;
                },
                _ = sigpipe => {
                    tracing::debug!("Got SIGPIPE (and ignored)");
                    continue
                }
            }
        }
    }))
}

fn unite<R, W>(reader: R, writer: W) -> Unite<R, W> {
    Unite { reader, writer }
}

#[pin_project]
struct Unite<R, W> {
    #[pin]
    reader: R,
    #[pin]
    writer: W,
}

impl<R, W> AsyncRead for Unite<R, W>
where
    R: AsyncRead,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        dst: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.project().reader.poll_read(cx, dst)
    }

    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        dst: &mut [IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        self.project().reader.poll_read_vectored(cx, dst)
    }
}

impl<R, W> AsyncWrite for Unite<R, W>
where
    W: AsyncWrite,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        src: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.project().writer.poll_write(cx, src)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        src: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.project().writer.poll_write_vectored(cx, src)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        self.project().writer.poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        self.project().writer.poll_close(cx)
    }
}
