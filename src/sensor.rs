//! Background thread that keeps a head-tracking stream open and hands out
//! frames. It reconnects on its own when the AirPods go away and come back.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::aap::{ACTIVATION_STREAM, Link, MOTION_STREAM};
use crate::bluez;
use crate::frame::{self, Frame};

#[derive(Debug)]
pub enum Event {
    Connecting(String),
    Connected { name: String, address: String },
    Frame(Frame),
    Ears(bool, bool),
    Lost(String),
}

pub struct Sensor {
    pub rx: Receiver<Event>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Sensor {
    pub fn spawn(device: Option<String>, interval_us: u32) -> Sensor {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::Builder::new()
            .name("mutix-sensor".into())
            .spawn(move || run(device, interval_us, &tx, &flag))
            .expect("cannot start the sensor thread");
        Sensor {
            rx,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Sensor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn pause(stop: &AtomicBool, d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn run(device: Option<String>, interval_us: u32, tx: &Sender<Event>, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        let dev = match bluez::pick(device.as_deref()) {
            Ok(d) => d,
            Err(e) => {
                let _ = tx.send(Event::Lost(e.to_string()));
                pause(stop, Duration::from_secs(3));
                continue;
            }
        };
        let _ = tx.send(Event::Connecting(dev.name.clone()));
        match session(&dev, interval_us, tx, stop) {
            Ok(()) => return,
            Err(e) => {
                if tx.send(Event::Lost(format!("{e:#}"))).is_err() {
                    return;
                }
                pause(stop, Duration::from_secs(2));
            }
        }
    }
}

/// Forwards one packet; returns true if it held at least one motion frame.
fn forward(packet: &[u8], tx: &Sender<Event>) -> Result<bool> {
    let frames = frame::parse(packet);
    let events: Vec<Event> = if !frames.is_empty() {
        frames.into_iter().map(Event::Frame).collect()
    } else if let Some((l, r)) = frame::parse_ears(packet) {
        vec![Event::Ears(l, r)]
    } else {
        return Ok(false);
    };
    let is_frame = matches!(events[0], Event::Frame(_));
    for event in events {
        if tx.send(event).is_err() {
            bail!("nobody is listening any more");
        }
    }
    Ok(is_frame)
}

/// Activation stream, then motion stream; true once a sample arrived.
fn start_motion(link: &mut Link, interval_us: u32, tx: &Sender<Event>, buf: &mut [u8]) -> Result<bool> {
    link.start_tracking(ACTIVATION_STREAM, 40_000)?;
    let settle = Instant::now() + Duration::from_millis(300);
    while Instant::now() < settle {
        if let Some(n) = link.recv(buf, Duration::from_millis(100))? {
            forward(&buf[..n], tx)?;
        }
    }
    link.start_tracking(MOTION_STREAM, interval_us)?;
    let until = Instant::now() + Duration::from_secs(2);
    let mut got = false;
    while Instant::now() < until && !got {
        if let Some(n) = link.recv(buf, Duration::from_millis(100))? {
            got = forward(&buf[..n], tx)?;
        }
    }
    link.stop_tracking(ACTIVATION_STREAM)?;
    Ok(got)
}

fn session(dev: &bluez::Device, interval_us: u32, tx: &Sender<Event>, stop: &AtomicBool) -> Result<()> {
    let mut link = Link::connect(&dev.address, Duration::from_secs(8))?;
    link.hello()?;
    let mut buf = [0u8; 2048];

    if !start_motion(&mut link, interval_us, tx, &mut buf)? {
        bail!("the AirPods send no motion data; put both of them in your ears");
    }
    let _ = tx.send(Event::Connected {
        name: dev.name.clone(),
        address: dev.address.clone(),
    });

    let mut last_frame = Instant::now();
    let mut last_request = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if let Some(n) = link.recv(&mut buf, Duration::from_millis(200))?
            && forward(&buf[..n], tx)?
        {
            last_frame = Instant::now();
        }
        // The stream pauses when a bud leaves the ear; asking again resumes it.
        if last_frame.elapsed() > Duration::from_secs(2) && last_request.elapsed() > Duration::from_secs(2) {
            if start_motion(&mut link, interval_us, tx, &mut buf)? {
                last_frame = Instant::now();
            }
            last_request = Instant::now();
        }
    }
    // No stop request on the way out: the motion stream is shared by every
    // connection to the AirPods, so stopping it would also cut a `mutix
    // listen` or a training session still running. Closing the channel is
    // enough.
    Ok(())
}
