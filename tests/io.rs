#![cfg(feature = "io")]

use std::future::{Future, poll_fn};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::task::Poll;
use std::thread;
use std::time::Duration;

use zrun::Runtime;
use zrun::io::Poller;

#[cfg(unix)]
#[test]
fn caller_driven_pipe_delivers_writes_larger_than_kernel_capacity() {
    use zrun::io::{AsyncFd, FdInterest, Nonblocking};

    let mut runtime = Runtime::new();
    let mut poller = Poller::new().expect("create poller");
    let (read_end, write_end) = std::io::pipe().expect("create stdin pipe");
    let mut reader =
        AsyncFd::new(poller.handle(), read_end, Nonblocking::Set).expect("register reader");
    let mut writer = AsyncFd::with_interest(
        poller.handle(),
        write_end,
        Nonblocking::Set,
        FdInterest::Writable,
    )
    .expect("register writer");
    let bytes: Vec<u8> = (0..(2 * 1024 * 1024))
        .map(|index| (index % 251) as u8)
        .collect();
    let expected = bytes.clone();

    let sent = runtime
        .spawn(async move {
            let mut offset = 0;
            let mut writes = 0;
            while offset < bytes.len() {
                let count = writer.write(&bytes[offset..]).await.expect("write pipe");
                assert_ne!(count, 0, "nonempty input should make progress");
                offset += count;
                writes += 1;
            }
            writes
        })
        .expect("spawn writer");
    let (received, writes) = runtime
        .block_on_with_io(&mut poller, async {
            let mut received = Vec::new();
            let mut buf = [0; 8192];
            loop {
                let count = reader.read(&mut buf).await.expect("read pipe");
                if count == 0 {
                    break;
                }
                received.extend_from_slice(&buf[..count]);
            }
            (received, sent.await.expect("complete writer"))
        })
        .expect("drive pipe I/O");
    assert_eq!(received, expected);
    assert!(writes > 1, "exercise partial writes through a bounded pipe");
}

#[cfg(all(unix, feature = "process"))]
#[test]
fn process_exit_notification_preserves_status_for_the_supervisor() {
    use std::process::{Command, Stdio};

    let mut runtime = Runtime::new();
    let timer = runtime.timer();
    let mut poller = Poller::new().expect("create poller");
    let handle = poller.handle().clone();
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "IFS= read -r line; printf '%s' \"$line\"; exit 37"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = zrun::process::spawn(&mut command, &handle).expect("launch shell");
    let mut stdin = child.take_stdin().unwrap().expect("stdin is piped");
    let mut stdout = child.take_stdout().unwrap().expect("stdout is piped");
    assert!(child.try_exit().unwrap().is_none());
    runtime
        .block_on_with_io(&mut poller, async {
            let message = b"literal input $value\n";
            assert_eq!(
                stdin.write(message).await.expect("write child stdin"),
                message.len()
            );
            drop(stdin);
            let observed = child
                .wait_for_exit(&timer)
                .await
                .expect("observe the real child exit");
            assert_eq!(observed.code(), Some(37));
            let mut output = Vec::new();
            let mut scratch = [0; 32];
            loop {
                let count = stdout.read(&mut scratch).await.expect("read stdout");
                if count == 0 {
                    break;
                }
                output.extend_from_slice(&scratch[..count]);
            }
            assert_eq!(output, &message[..message.len() - 1]);
            assert_eq!(child.wait(&timer).await.expect("reap child"), observed);
            assert_eq!(child.wait(&timer).await.expect("cached status"), observed);
        })
        .expect("drive process I/O");
}

#[test]
fn foreign_admission_interrupts_a_caller_driven_poll() {
    let mut runtime = Runtime::new();
    let mut poller = Poller::new().expect("create poller");
    let spawner = runtime.spawner();
    let (eligible, wait_root) = mpsc::channel();
    let (send, receive) = zrun::sync::channel();
    let producer = thread::spawn(move || {
        wait_root.recv().expect("root started polling");
        thread::sleep(Duration::from_millis(20));
        spawner
            .spawn(async move {
                send.send(42).expect("root is still waiting");
            })
            .expect("admit across threads")
    });
    let result = runtime
        .block_on_with_io(&mut poller, async {
            eligible.send(()).expect("signal root entry");
            receive.await
        })
        .expect("poll caller-driven runtime")
        .expect("get result");
    assert_eq!(result, 42);
    runtime
        .block_on(producer.join().expect("join producer"))
        .expect("finish producer task");
}

#[test]
fn caller_driven_tcp_progresses_while_tasks_remain_runnable() {
    let mut runtime = Runtime::new();
    let mut poller = Poller::new().expect("create poller");
    let mut listener = poller
        .handle()
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .expect("bind test server");
    let address = listener.local_addr().expect("get server address");
    let polls = Arc::new(AtomicUsize::new(0));
    let active = polls.clone();
    let busy = runtime
        .spawn(poll_fn(move |cx| {
            active.fetch_add(1, Ordering::Relaxed);
            cx.waker().wake_by_ref();
            Poll::<()>::Pending
        }))
        .expect("admit always runnable task");
    let client = thread::spawn(move || {
        let mut conn = TcpStream::connect(address).expect("connect");
        conn.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read deadline");
        conn.write_all(b"test").expect("send input");
        let mut received = [0; 4];
        conn.read_exact(&mut received).expect("receive echo");
        received
    });
    runtime
        .block_on_with_io(&mut poller, async {
            let (mut stream, _) = listener.accept().await?;
            let mut data = [0; 4];
            let mut offset = 0;
            while offset < data.len() {
                let count = stream.read(&mut data[offset..]).await?;
                if count == 0 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                offset += count;
            }
            stream.write_all(&data).await
        })
        .expect("poll socket")
        .expect("serve echo");
    assert_eq!(client.join().expect("join TCP client"), *b"test");
    assert_ne!(polls.load(Ordering::Relaxed), 0);
    busy.abort();
    assert!(
        runtime
            .block_on(busy)
            .expect_err("cancel running task")
            .is_cancelled()
    );
}

#[test]
fn tcp_read_exact_resumes_after_a_partial_read_and_reports_early_eof() {
    let mut runtime = Runtime::new();
    let mut poller = Poller::new().unwrap();
    let mut listener = poller
        .handle()
        .bind("127.0.0.1:0".parse().unwrap())
        .unwrap();
    let address = listener.local_addr().unwrap();
    let (prefix_written, prefix_ready) = mpsc::channel();
    let (send_suffix, wait_suffix) = mpsc::channel();
    let client = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(b"ab").unwrap();
        prefix_written.send(()).unwrap();
        wait_suffix.recv_timeout(Duration::from_secs(5)).unwrap();
        stream.write_all(b"cde").unwrap();
    });
    prefix_ready.recv_timeout(Duration::from_secs(5)).unwrap();
    runtime
        .block_on_with_io(&mut poller, async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut exact = [0; 4];
            {
                let mut read = std::pin::pin!(stream.read_exact(&mut exact));
                poll_fn(|cx| {
                    assert!(read.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                send_suffix.send(()).unwrap();
                read.await.unwrap();
            }
            assert_eq!(&exact, b"abcd");
            let mut short = [0; 2];
            assert_eq!(
                stream.read_exact(&mut short).await.unwrap_err().kind(),
                std::io::ErrorKind::UnexpectedEof
            );
            assert_eq!(short[0], b'e');
            stream.read_exact(&mut []).await.unwrap();
        })
        .unwrap();
    client.join().unwrap();
}

#[test]
fn cancelling_tcp_read_exact_allows_a_new_reader_to_receive_the_suffix() {
    let mut runtime = Runtime::new();
    let mut poller = Poller::new().unwrap();
    let mut listener = poller
        .handle()
        .bind("127.0.0.1:0".parse().unwrap())
        .unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = TcpStream::connect(address).unwrap();
    client.write_all(b"ab").unwrap();
    runtime
        .block_on_with_io(&mut poller, async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut partial = [0; 4];
            {
                let mut read = std::pin::pin!(stream.read_exact(&mut partial));
                poll_fn(|cx| {
                    assert!(read.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
            }
            let consumed = partial.iter().take_while(|&&byte| byte != 0).count();
            assert!(consumed <= 2);
            assert_eq!(&partial[..consumed], &b"ab"[..consumed]);
            client.write_all(b"cd").unwrap();
            stream.read_exact(&mut partial[consumed..]).await.unwrap();
            assert_eq!(&partial, b"abcd");
        })
        .unwrap();
}

#[test]
fn pool_driven_io_progresses_under_ready_work_and_closes_on_shutdown() {
    use std::sync::atomic::AtomicBool;

    let (mut pool, handle) = zrun::Pool::with_max_tasks_and_io(4, 32).unwrap();
    let mut listener = handle.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let running = stop.clone();
    let busy = pool
        .spawn(async move {
            while !running.load(Ordering::Acquire) {
                zrun::yield_now().await;
            }
        })
        .unwrap();
    let echo = pool
        .spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut bytes = [0; 5];
            let mut received = 0;
            while received < bytes.len() {
                let count = stream.read(&mut bytes[received..]).await?;
                if count == 0 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                received += count;
            }
            stream.write_all(&bytes).await
        })
        .unwrap();
    let mut client = TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    client.write_all(b"hello").unwrap();
    let mut response = [0; 5];
    let read = client.read_exact(&mut response);
    stop.store(true, Ordering::Release);
    read.unwrap();
    assert_eq!(&response, b"hello");
    pool.block_on(echo).unwrap().unwrap();
    pool.block_on(busy).unwrap();
    pool.shutdown();
    assert_eq!(
        handle
            .bind("127.0.0.1:0".parse().unwrap())
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::BrokenPipe
    );
}
