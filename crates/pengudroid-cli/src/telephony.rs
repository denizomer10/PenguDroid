//! `pengudroid telephony` — transact against the `phone` service and decode the
//! replies.
//!
//! This is the observable end of the telephony work: it looks up the `phone`
//! service in the broker, sends each `ITelephony` transaction with its real AIDL
//! code, and decodes the `Parcel` reply the way a caller would. It is a
//! diagnostic, not a phone stack — the device it reports is a no-SIM, radio-off
//! one.

use pengudroid_proto::Message;

use crate::client::Client;

/// How to decode a method's reply.
#[derive(Clone, Copy)]
enum Kind {
    Int,
    Bool,
    /// A `String` that may be `null`.
    String,
    /// A nullable `Parcelable`.
    Parcelable,
    /// A `List` (only its element count is decoded).
    List,
}

struct Method {
    code: u32,
    name: &'static str,
    kind: Kind,
}

/// The `ITelephony` subset the daemon implements, in AIDL declaration order.
const METHODS: &[Method] = &[
    Method { code: 3, name: "isRadioOn", kind: Kind::Bool },
    Method { code: 34, name: "getCallState", kind: Kind::Int },
    Method { code: 36, name: "getDataActivity", kind: Kind::Int },
    Method { code: 38, name: "getDataState", kind: Kind::Int },
    Method { code: 40, name: "getActivePhoneType", kind: Kind::Int },
    Method { code: 64, name: "getNetworkTypeForSubscriber", kind: Kind::Int },
    Method { code: 65, name: "getDataNetworkType", kind: Kind::Int },
    Method { code: 67, name: "getVoiceNetworkTypeForSubscriber", kind: Kind::Int },
    Method { code: 68, name: "hasIccCard", kind: Kind::Bool },
    Method { code: 69, name: "hasIccCardUsingSlotIndex", kind: Kind::Bool },
    Method { code: 72, name: "getAllCellInfo", kind: Kind::List },
    Method { code: 146, name: "getDeviceId", kind: Kind::String },
    Method { code: 147, name: "getDeviceIdWithFeature", kind: Kind::String },
    Method { code: 148, name: "getImeiForSlot", kind: Kind::String },
    Method { code: 149, name: "getPrimaryImei", kind: Kind::String },
    Method { code: 151, name: "getMeidForSlot", kind: Kind::String },
    Method { code: 159, name: "getServiceStateForSlot", kind: Kind::Parcelable },
    Method { code: 190, name: "getSignalStrength", kind: Kind::Parcelable },
];

/// Decode a reply `Parcel` into a printable string.
fn decode(kind: Kind, data: &[u8]) -> String {
    let mut r = Reader { data, pos: 0 };
    match kind {
        Kind::Int => match r.i32() {
            Some(v) => v.to_string(),
            None => "<truncated>".into(),
        },
        Kind::Bool => match r.i32() {
            Some(v) => (v != 0).to_string(),
            None => "<truncated>".into(),
        },
        Kind::String => match r.i32() {
            Some(-1) => "null".into(),
            Some(n) if n >= 0 => {
                let bytes = match r.take(n as usize * 2 + 2) {
                    Some(b) => b,
                    None => return "<truncated>".into(),
                };
                let units: Vec<u16> = bytes[..n as usize * 2]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                format!("\"{}\"", String::from_utf16_lossy(&units))
            }
            _ => "<truncated>".into(),
        },
        Kind::Parcelable => match r.i32() {
            Some(0) => "null".into(),
            Some(_) => "<object>".into(),
            None => "<truncated>".into(),
        },
        Kind::List => match r.i32() {
            Some(n) => format!("[{n} items]"),
            None => "<truncated>".into(),
        },
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    fn i32(&mut self) -> Option<i32> {
        let b = self.take(4)?;
        Some(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

pub fn run(_args: &[String]) -> i32 {
    let client = match Client::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pengudroid: {e}");
            return 1;
        }
    };
    if let Err(e) = client.handshake(pengudroid_proto::Role::Cli) {
        eprintln!("pengudroid: {e}");
        return 1;
    }

    let handle = match client.request(&Message::ListServices) {
        Ok((Message::ServiceList { services }, _)) => {
            match services.into_iter().find(|(name, _, _)| name == "phone") {
                Some((_, _, h)) => h,
                None => {
                    eprintln!("pengudroid: the daemon has no 'phone' service");
                    return 1;
                }
            }
        }
        Ok((other, _)) => {
            eprintln!("pengudroid: unexpected reply: {other:?}");
            return 1;
        }
        Err(e) => {
            eprintln!("pengudroid: {e}");
            return 1;
        }
    };

    println!("{:<32} {:>5}  {}", "METHOD", "CODE", "REPLY");
    let mut failures = 0;
    for m in METHODS {
        let req = Message::BinderTransact { handle, code: m.code, flags: 0, data: Vec::new() };
        match client.request(&req) {
            Ok((Message::BinderReply { status, data }, _)) => {
                if status == 0 {
                    println!("{:<32} {:>5}  {}", m.name, m.code, decode(m.kind, &data));
                } else {
                    println!("{:<32} {:>5}  status {status}", m.name, m.code);
                    failures += 1;
                }
            }
            Ok((other, _)) => {
                println!("{:<32} {:>5}  unexpected reply: {other:?}", m.name, m.code);
                failures += 1;
            }
            Err(e) => {
                println!("{:<32} {:>5}  error: {e}", m.name, m.code);
                failures += 1;
            }
        }
    }
    if failures == 0 {
        0
    } else {
        1
    }
}
