//! `cargo test --test timeout`: a loopback port whose connection attempt times out.
//!
//! It is its own test binary, and cargo runs test binaries one after another: every
//! doctor run scans the shared network namespace, so this port would make the other
//! tests' service scans incomplete too.
#![allow(dead_code, unused_imports)]

include!("support/fixture.rs");

#[test]
fn a_port_that_times_out_leaves_the_service_scan_unknown() {
    // Review R3, item A: a timeout is not a connection. A listener whose accept queue
    // is full has its SYNs dropped, so a connection attempt to it times out.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // SAFETY: shrinks the accept queue of a socket we own.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
    let mut queued = Vec::new();
    let timed_out = (0..4).any(|_| {
        match std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
            Ok(stream) => {
                queued.push(stream);
                false
            }
            Err(err) => err.kind() == std::io::ErrorKind::TimedOut,
        }
    });
    // The control: on this host, the port does time out.
    assert!(
        timed_out,
        "the full accept queue did not make connections time out"
    );

    let fixture = Fixture::new();
    let run = fixture.doctor(&fixture.harness("ok"));
    let reachable = run.item("wrapper-reachable-services");
    assert!(
        reachable.starts_with("unknown: the scan was incomplete: ")
            && reachable.contains(&format!("cannot try 127.0.0.1:{port}")),
        "{reachable}"
    );
    assert_eq!(run.code, 1);
    drop(queued);
}
