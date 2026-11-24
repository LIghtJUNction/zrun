#![cfg(feature = "io")]

use std::future::poll_fn;
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
