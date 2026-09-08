//! Bounded synchronous named-pipe I/O for interactive daemon clients.
//! PIPE_NOWAIT lets the existing IPC deadline bound a stalled peer; hooks
//! continue to append to the spool without opening this transport.
use std::{
    cell::Cell,
    fs::File,
    io::{self, Read, Write},
    os::windows::io::AsRawHandle,
    time::{Duration, Instant},
};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetNamedPipeHandleState(
        pipe: *mut std::ffi::c_void,
        mode: *const u32,
        max_collection_count: *const u32,
        collect_data_timeout: *const u32,
    ) -> i32;
    fn ReadFile(
        file: *mut std::ffi::c_void,
        buffer: *mut std::ffi::c_void,
        length: u32,
        read: *mut u32,
        overlapped: *mut std::ffi::c_void,
    ) -> i32;
}

pub(crate) struct Pipe {
    file: File,
    deadline: Cell<Instant>,
}
impl Pipe {
    pub(crate) fn open(name: &str, timeout: Duration) -> io::Result<Self> {
        let deadline = Instant::now() + timeout;
        let file = loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(name)
            {
                Ok(file) => break file,
                Err(e) if e.raw_os_error() == Some(231) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(e),
            }
        };
        let mode = 1_u32; // PIPE_NOWAIT; byte read mode remains unchanged.
        // SAFETY: file owns a valid pipe handle; mode is valid for the call,
        // and the two unused collection parameters are null as documented.
        if unsafe {
            SetNamedPipeHandleState(
                file.as_raw_handle(),
                &mode,
                std::ptr::null(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            file,
            deadline: Cell::new(deadline),
        })
    }
    pub(crate) fn set_timeout(&self, duration: Option<Duration>) -> io::Result<()> {
        self.deadline
            .set(Instant::now() + duration.unwrap_or(Duration::from_secs(30)));
        Ok(())
    }
    fn retry(
        &mut self,
        mut operation: impl FnMut(&mut File) -> io::Result<usize>,
        write: bool,
    ) -> io::Result<usize> {
        loop {
            if Instant::now() >= self.deadline.get() {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            match operation(&mut self.file) {
                Ok(0) if write => {}
                Err(e) if matches!(e.raw_os_error(), Some(232 | 231)) => {}
                result => return result,
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
impl Read for Pipe {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        // std::fs::File maps an empty nonblocking pipe (ERROR_NO_DATA) to
        // EOF on Windows. Preserve the native error so retry waits for the
        // daemon's reply instead of closing a healthy connection early.
        self.retry(
            |file| {
                let mut read = 0;
                // SAFETY: this is an owned synchronous pipe handle; bytes
                // and read remain valid until the nonblocking call returns.
                let ok = unsafe {
                    ReadFile(
                        file.as_raw_handle(),
                        bytes.as_mut_ptr().cast(),
                        bytes.len().min(u32::MAX as usize) as u32,
                        &mut read,
                        std::ptr::null_mut(),
                    )
                };
                if ok != 0 {
                    Ok(read as usize)
                } else {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(109) {
                        Ok(0) // ERROR_BROKEN_PIPE is a real peer disconnect.
                    } else {
                        Err(error)
                    }
                }
            },
            false,
        )
    }
}
impl Write for Pipe {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.retry(|file| file.write(bytes), true)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_delayed_reply_is_read_before_the_peer_disconnects() {
        let name = format!(r"\\.\pipe\attemptdb-reply-{}", uuid::Uuid::new_v4());
        let server_name = name.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut pipe = tokio::net::windows::named_pipe::ServerOptions::new()
                        .first_pipe_instance(true)
                        .create(&server_name)
                        .unwrap();
                    ready_tx.send(()).unwrap();
                    pipe.connect().await.unwrap();
                    let mut request = [0; 4];
                    pipe.read_exact(&mut request).await.unwrap();
                    assert_eq!(&request, b"ping");
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    pipe.write_all(b"pong").await.unwrap();
                    let mut ack = [0; 1];
                    pipe.read_exact(&mut ack).await.unwrap();
                });
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut pipe = Pipe::open(&name, Duration::from_secs(5)).unwrap();
        pipe.write_all(b"ping").unwrap();
        let mut reply = [0; 4];
        pipe.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"pong");
        pipe.write_all(b"!").unwrap();
        server.join().unwrap();
        assert_eq!(pipe.read(&mut [0; 1]).unwrap(), 0);
    }
    #[test]
    fn a_connected_peer_that_never_answers_cannot_hang_the_installer() {
        let name = format!(r"\\.\pipe\attemptdb-timeout-{}", uuid::Uuid::new_v4());
        let server_name = name.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let pipe = tokio::net::windows::named_pipe::ServerOptions::new()
                        .first_pipe_instance(true)
                        .create(&server_name)
                        .unwrap();
                    ready_tx.send(()).unwrap();
                    pipe.connect().await.unwrap();
                    tokio::time::sleep(Duration::from_secs(1)).await;
                });
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut pipe = Pipe::open(&name, Duration::from_millis(250)).unwrap();
        pipe.set_timeout(Some(Duration::from_millis(80))).unwrap();
        let start = Instant::now();
        assert_eq!(
            pipe.read(&mut [0_u8; 1]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_millis(700));
        drop(pipe);
        server.join().unwrap();
    }
}
