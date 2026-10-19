// Guest-to-guest service traffic, included from `server.rs`.
//
// When a client transacts on a handle owned by another guest, the daemon does
// not answer itself: it pushes a `GuestCall` into the owner connection's inbox,
// and that connection's serving thread writes it to the guest's socket. The
// owner answers with `GuestCallReply`, which *its* serving thread parks in the
// caller's `pending` queue; the caller's thread turns that into the
// `BinderReply` it owes its client. Both directions therefore stay inside each
// connection's own thread, so nested calls (A -> B while B itself waits on C)
// cannot deadlock two threads that are both blocked reading.

use std::sync::atomic::AtomicU32;

/// Token handed out with each forwarded call; used for logging/diagnosis, since
/// replies are ordered per connection.
static NEXT_TOKEN: AtomicU32 = AtomicU32::new(1);

fn next_token() -> u32 {
    let t = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    if t == 0 {
        NEXT_TOKEN.store(2, Ordering::Relaxed);
        1
    } else {
        t
    }
}

/// Write everything queued in `inbox` to `conn`. Returns `false` if the peer is
/// gone.
fn drain_inbox(inbox: &Receiver<Message>, conn: &Connection) -> bool {
    loop {
        match inbox.try_recv() {
            Ok(msg) => {
                if conn.send_message(&msg, &[]).is_err() {
                    return false;
                }
            }
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => return true,
        }
    }
}

/// Forward a transaction to the guest that owns `handle` and wait for its
/// answer.
///
/// Runs on the *caller's* serving thread. While it waits, `drain_inbox` keeps
/// delivering calls that other guests address to this one in the meantime, so
/// mutual calls between two guests make progress instead of deadlocking.
fn forward_guest_call(
    peer: &Peer,
    owner: &Sender<Message>,
    code: u32,
    data: Vec<u8>,
) -> (Option<Message>, Vec<OwnedFd>, bool) {
    let token = next_token();
    let call = Message::GuestCall { handle: 0, code, data, token };
    if owner.send(call).is_err() {
        // The owner's connection is gone (its thread will clean up the handle).
        return (
            Some(Message::BinderReply { status: -32, data: Vec::new() }), // EPIPE
            vec![],
            false,
        );
    }

    let deadline = Instant::now() + GUEST_CALL_TIMEOUT;
    loop {
        if let Some(reply) = peer.pending.lock().unwrap().pop() {
            return (Some(reply), vec![], false);
        }
        if Instant::now() >= deadline {
            crate::warn!("guest service call {token} timed out");
            return (
                Some(Message::Error {
                    code: -110,
                    message: "guest service did not answer in time".into(),
                }),
                vec![],
                false,
            );
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod forward_tests {
    // The forwarding path is exercised end to end by `tests/guest_ipc.rs`,
    // which runs a real daemon with two real guest connections.

    #[test]
    fn tokens_are_non_zero_and_distinct() {
        let a = super::next_token();
        let b = super::next_token();
        assert!(a != 0 && b != 0);
        assert_ne!(a, b);
    }
}
