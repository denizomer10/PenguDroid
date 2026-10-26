//! Connection-local hardware decoding. Native driver objects never leave the
//! serving thread; only exported descriptors and release flags cross into graphics.
use super::{Daemon, Message};
use pengudroid_hostgpu::video::{Decoder, HardwareFrame, ReceiveStatus, SendStatus};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};

const MAX_HELD: usize = 4;

struct HeldFrame {
    frame: HardwareFrame,
    released: Option<Arc<AtomicBool>>,
}
struct Session {
    decoder: Decoder,
    held: Vec<HeldFrame>,
    surface: u32,
    decoded: u64,
    presented: u64,
    last_pts_us: Option<i64>,
    eos_sent: bool,
    eos_received: bool,
}
#[derive(Default)]
pub(super) struct VideoConnection { session: Option<Session> }

pub(super) fn is_request(message: &Message) -> bool {
    matches!(message, Message::VideoDecoderOpen { .. } | Message::VideoPacket { .. }
        | Message::VideoDecoderFlush | Message::VideoDecoderClose | Message::VideoDecoderPoll)
}
impl VideoConnection {
    pub(super) fn handle(&mut self, daemon: &Daemon, pid: i32, message: Message) -> Message {
        match self.request(daemon, pid, message) {
            Ok(reply) => reply,
            Err(message) => Message::Error { code: -22, message },
        }
    }
    fn request(&mut self, daemon: &Daemon, pid: i32, message: Message) -> Result<Message, String> {
        if let Message::VideoDecoderOpen { surface_id, codec, device, extra } = message {
            if self.session.is_some() { return Err("close the existing hardware decoder first".into()); }
            {
                let graphics = daemon.graphics.lock().unwrap();
                let graphics = graphics.as_ref().ok_or("graphics unavailable")?;
                if !graphics.contains_surface(surface_id)
                    || daemon.surface_owner.lock().unwrap().get(&surface_id) != Some(&pid) {
                    return Err("hardware decoder surface is unavailable or belongs to another process".into());
                }
            }
            let decoder = Decoder::open(&codec, std::path::Path::new(&device), &extra)?;
            self.session = Some(Session { decoder, held: Vec::new(), surface: surface_id,
                decoded: 0, presented: 0, last_pts_us: None, eos_sent: false, eos_received: false });
            return Ok(Message::VideoDecoderReady);
        }
        if message == Message::VideoDecoderClose {
            if let Some(session) = self.session.take() {
                // Detach the import. Closing the decoder stops pool reuse, and
                // exported DMA-BUF references keep compositor storage alive.
                if let Some(graphics) = daemon.graphics.lock().unwrap().as_mut() {
                    graphics.present(session.surface);
                }
                drop(session);
            }
            return Ok(Message::Ack);
        }
        let session = self.session.as_mut().ok_or("no hardware decoder on this connection")?;
        match message {
            Message::VideoDecoderFlush => {
                session.decoder.flush();
                // Submitted frames stay retained until actual release. Pending
                // frames from the previous stream generation can be discarded.
                session.held.retain(|frame| frame.released.is_some());
                session.decoded = 0; session.presented = 0; session.last_pts_us = None;
                session.eos_sent = false; session.eos_received = false;
                Ok(session.status(false))
            }
            Message::VideoPacket { pts_us, data, end_of_stream } => {
                if (end_of_stream && !data.is_empty()) || (!end_of_stream && data.is_empty()) {
                    return Err("send one nonempty access unit or an empty EOS; use Poll to drain".into());
                }
                if session.eos_sent { return Err("decoder is draining; flush before sending another packet".into()); }
                session.pump(daemon, pid)?;
                if session.blocked() { return Ok(session.status(false)); }
                let accepted = if end_of_stream {
                    session.decoder.end_of_stream()? == SendStatus::Accepted
                } else { session.decoder.send(&data, pts_us)? == SendStatus::Accepted };
                if accepted && end_of_stream { session.eos_sent = true; }
                session.drain(daemon, pid)?;
                Ok(session.status(accepted))
            }
            Message::VideoDecoderPoll => {
                session.drain(daemon, pid)?;
                Ok(session.status(false))
            }
            _ => Err("unexpected hardware decoder command".into()),
        }
    }
}
impl Session {
    fn blocked(&self) -> bool {
        self.held.len() >= MAX_HELD || self.held.iter().any(|frame| frame.released.is_none())
    }
    fn status(&self, input_accepted: bool) -> Message {
        Message::VideoDecodeStatus { input_accepted, decoded: self.decoded,
            presented: self.presented, end_of_stream: self.eos_received,
            blocked: self.blocked(), last_pts_us: self.last_pts_us }
    }
    fn pump(&mut self, daemon: &Daemon, pid: i32) -> Result<(), String> {
        // Read releases before pruning. Only graphics holds the Wayland queue.
        let mut guard = daemon.graphics.lock().unwrap();
        let graphics = guard.as_mut().ok_or("graphics unavailable")?;
        graphics.poll();
        if !graphics.contains_surface(self.surface)
            || daemon.surface_owner.lock().unwrap().get(&self.surface) != Some(&pid) {
            return Err("hardware decoder surface was retired".into());
        }
        self.held.retain(|frame| !frame.released.as_ref().is_some_and(|flag| flag.load(Ordering::Acquire)));
        for held in &mut self.held {
            if held.released.is_some() { continue; }
            let export = held.frame.export()?;
            let Some(released) = graphics.present_video_dmabuf(self.surface, export)? else { break };
            held.released = Some(released);
            self.presented += 1;
            self.last_pts_us = Some(held.frame.pts_us());
        }
        Ok(())
    }
    fn drain(&mut self, daemon: &Daemon, pid: i32) -> Result<(), String> {
        self.pump(daemon, pid)?;
        while !self.blocked() && !self.eos_received {
            match self.decoder.receive()? {
                ReceiveStatus::Frame(frame) => {
                    self.decoded += 1;
                    self.held.push(HeldFrame { frame, released: None });
                    self.pump(daemon, pid)?;
                }
                ReceiveStatus::NeedsInput => break,
                ReceiveStatus::EndOfStream => self.eos_received = true,
                ReceiveStatus::FramesHeld => break,
            }
        }
        Ok(())
    }
}
