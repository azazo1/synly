//! 阻塞平台 IO 到异步字节流的有界桥接.

use super::stream::ByteStream;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::PollSender;

pub const IO_FRAGMENT_BYTES: usize = 1024;

/// read 和 write_all 可以并行, close 必须幂等且唤醒全部阻塞 IO.
pub trait BlockingByteIo: Send + Sync + 'static {
    fn read(&self, buffer: &mut [u8]) -> io::Result<usize>;
    fn write_all(&self, bytes: &[u8]) -> io::Result<()>;
    fn close(&self);
}

struct WriteRequest {
    bytes: Vec<u8>,
    acknowledged: oneshot::Sender<io::Result<()>>,
}

struct CloseOnDrop(Arc<dyn BlockingByteIo>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) { self.0.close(); }
}

/// 最多一个待确认写片段. flush 等到原生写入完成, 不仅是进入 Rust 队列.
/// 接收队列最多两个片段, 不因应用读取变慢而积压无限的蓝牙数据.
pub fn open(io: Arc<dyn BlockingByteIo>) -> ByteStream {
    let (outgoing, mut writes) = mpsc::channel::<WriteRequest>(1);
    let (incoming, reads) = mpsc::channel(2);
    let reader_io = io.clone();
    let reader = tokio::task::spawn_blocking(move || {
        let _close = CloseOnDrop(reader_io.clone());
        loop {
            let mut bytes = vec![0; IO_FRAGMENT_BYTES];
            let result = reader_io.read(&mut bytes);
            let finished = !matches!(&result, Ok(size) if *size > 0 && *size <= bytes.len());
            let data = match result {
                Ok(size) if size <= bytes.len() => { bytes.truncate(size); Ok(bytes) }
                Ok(_) => Err(io::Error::other("原生读取超出缓冲区大小")),
                Err(error) => { tracing::debug!(%error, "原生字节流读取结束"); Err(error) }
            };
            if finished { reader_io.close(); }
            if incoming.blocking_send(data).is_err() || finished { return; }
        }
    });
    let writer_io = io.clone();
    let writer = tokio::task::spawn_blocking(move || {
        let _close = CloseOnDrop(writer_io.clone());
        while let Some(request) = writes.blocking_recv() {
            let result = writer_io.write_all(&request.bytes);
            let failed = result.is_err();
            if let Err(error) = &result { tracing::debug!(%error, "原生字节流写入结束"); }
            let abandoned = request.acknowledged.send(result).is_err();
            if failed || abandoned { return; }
        }
    });
    ByteStream::new(BridgeStream {
        io,
        outgoing: PollSender::new(outgoing),
        incoming: reads,
        acknowledged: None,
        buffer: Vec::new(),
        offset: 0,
        eof: false,
        workers: vec![reader, writer],
    })
}

struct BridgeStream {
    io: Arc<dyn BlockingByteIo>,
    outgoing: PollSender<WriteRequest>,
    incoming: mpsc::Receiver<io::Result<Vec<u8>>>,
    acknowledged: Option<oneshot::Receiver<io::Result<()>>>,
    buffer: Vec<u8>,
    offset: usize,
    eof: bool,
    workers: Vec<JoinHandle<()>>,
}

impl Drop for BridgeStream {
    fn drop(&mut self) {
        self.io.close();
        self.outgoing.close();
        self.outgoing.abort_send();
        for worker in &self.workers { worker.abort(); }
    }
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "原生字节流已关闭")
}

impl BridgeStream {
    fn poll_acknowledged(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(receiver) = &mut self.acknowledged {
            let result = ready!(Pin::new(receiver).poll(cx));
            self.acknowledged = None;
            let result = result.map_err(|_| closed()).and_then(|result| result);
            if result.is_err() {
                self.outgoing.close();
                self.outgoing.abort_send();
                self.io.close();
            }
            result?;
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for BridgeStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, output: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if output.remaining() == 0 || self.eof { return Poll::Ready(Ok(())); }
        if self.offset == self.buffer.len() {
            self.buffer.clear();
            self.offset = 0;
            match ready!(self.incoming.poll_recv(cx)) {
                Some(Ok(bytes)) if !bytes.is_empty() => self.buffer = bytes,
                Some(Err(error)) => return Poll::Ready(Err(error)),
                _ => { self.eof = true; return Poll::Ready(Ok(())); }
            }
        }
        let size = output.remaining().min(self.buffer.len() - self.offset);
        output.put_slice(&self.buffer[self.offset..self.offset + size]);
        self.offset += size;
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for BridgeStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        ready!(self.poll_acknowledged(cx))?;
        if bytes.is_empty() { return Poll::Ready(Ok(0)); }
        ready!(self.outgoing.poll_reserve(cx)).map_err(|_| closed())?;
        let size = bytes.len().min(IO_FRAGMENT_BYTES);
        let (acknowledged, receiver) = oneshot::channel();
        self.outgoing.send_item(WriteRequest { bytes: bytes[..size].to_vec(), acknowledged }).map_err(|_| closed())?;
        self.acknowledged = Some(receiver);
        Poll::Ready(Ok(size))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_acknowledged(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_acknowledged(cx))?;
        self.outgoing.close();
        self.outgoing.abort_send();
        self.io.close();
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Notify;

    #[derive(Default)]
    struct State {
        closed: bool,
        release_write: bool,
        write_failure: Option<io::ErrorKind>,
        bytes: Vec<u8>,
        received: VecDeque<u8>,
    }

    #[derive(Default)]
    struct FakeIo {
        state: Mutex<State>,
        changed: Condvar,
        started: Notify,
        read_started: Notify,
        read_done: Notify,
        write_done: Notify,
        stopped: Notify,
    }

    impl BlockingByteIo for FakeIo {
        fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
            self.read_started.notify_one();
            let mut state = self.state.lock().unwrap();
            while !state.closed && state.received.is_empty() { state = self.changed.wait(state).unwrap(); }
            let size = buffer.len().min(state.received.len());
            for byte in &mut buffer[..size] { *byte = state.received.pop_front().unwrap(); }
            self.read_done.notify_one();
            Ok(size)
        }

        fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
            assert!(bytes.len() <= IO_FRAGMENT_BYTES);
            self.started.notify_one();
            let mut state = self.state.lock().unwrap();
            while !state.closed && !state.release_write { state = self.changed.wait(state).unwrap(); }
            self.write_done.notify_one();
            if state.closed { return Err(closed()); }
            if let Some(kind) = state.write_failure { return Err(io::Error::new(kind, "原生写入失败")); }
            state.bytes.extend_from_slice(bytes);
            Ok(())
        }

        fn close(&self) {
            self.state.lock().unwrap().closed = true;
            self.changed.notify_all();
            self.stopped.notify_one();
        }
    }

    #[tokio::test]
    async fn flush_waits_for_native_completion_and_large_writes_are_bounded() {
        let io = Arc::new(FakeIo::default());
        let mut stream = open(io.clone());
        stream.write_all(&vec![7; IO_FRAGMENT_BYTES]).await.unwrap();
        io.started.notified().await;
        assert!(tokio::time::timeout(Duration::from_millis(30), stream.flush()).await.is_err());
        assert!(io.state.lock().unwrap().bytes.is_empty());
        io.state.lock().unwrap().release_write = true;
        io.changed.notify_all();
        stream.flush().await.unwrap();
        stream.write_all(&vec![9; 64 * IO_FRAGMENT_BYTES]).await.unwrap();
        stream.flush().await.unwrap();
        assert_eq!(io.state.lock().unwrap().bytes.len(), 65 * IO_FRAGMENT_BYTES);
        stream.shutdown().await.unwrap();
        io.stopped.notified().await;
    }

    #[tokio::test]
    async fn dropping_stream_wakes_blocked_reader_and_writer() {
        let io = Arc::new(FakeIo::default());
        let mut stream = open(io.clone());
        stream.write_all(&[1, 2, 3]).await.unwrap();
        io.started.notified().await;
        io.read_started.notified().await;
        drop(stream);
        tokio::time::timeout(Duration::from_secs(1), async {
            io.read_done.notified().await;
            io.write_done.notified().await;
        }).await.unwrap();
        assert!(io.state.lock().unwrap().closed);
    }

    #[tokio::test]
    async fn native_write_failure_is_reported_and_closes_both_directions() {
        let io = Arc::new(FakeIo::default());
        {
            let mut state = io.state.lock().unwrap();
            state.release_write = true;
            state.write_failure = Some(io::ErrorKind::PermissionDenied);
        }
        let mut stream = open(io.clone());
        stream.write_all(&[1, 2, 3]).await.unwrap();
        assert_eq!(stream.flush().await.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(stream.write_all(&[4]).await.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        let mut bytes = [0; 1];
        assert_eq!(stream.read(&mut bytes).await.unwrap(), 0);
        assert!(io.state.lock().unwrap().closed);
    }

    #[tokio::test]
    async fn partial_reads_preserve_byte_order() {
        let io = Arc::new(FakeIo::default());
        io.state.lock().unwrap().received.extend(0u8..=255);
        let mut stream = open(io.clone());
        let mut received = vec![0; 256];
        for byte in &mut received { stream.read_exact(std::slice::from_mut(byte)).await.unwrap(); }
        assert_eq!(received, (0u8..=255).collect::<Vec<_>>());
        drop(stream);
    }
}
