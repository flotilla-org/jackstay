//! Independent, bounded semantic state and commands. Navigation URLs are
//! untrusted host input: the producer owns their interpretation and URL policy.
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{framing::Framed, local::Stream};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Artwork {
    Url { url: String },
    Icon { name: String },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Media {
    pub status: String,
    pub position: Option<f64>,
    pub rate: f64,
    pub duration: Option<f64>,
    pub title: Option<String>,
    pub artwork: Option<Artwork>,
    pub capabilities: BTreeMap<String, bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Navigation {
    pub url: Option<String>,
    pub title: Option<String>,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub loading: bool,
    pub capabilities: BTreeMap<String, bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Axis {
    pub scrollable: bool,
    pub content_length: f64,
    pub viewport_length: f64,
    pub position: f64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Scroll {
    pub x: Axis,
    pub y: Axis,
    pub capabilities: BTreeMap<String, bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Window {
    pub title: Option<String>,
    pub requested_size: Option<Size>,
    pub ready: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Presentation {
    pub visible: bool,
    pub preferred_size: Option<Size>,
    pub scale: f64,
    pub focused: bool,
}
impl Default for Presentation {
    fn default() -> Self {
        Self {
            visible: true,
            preferred_size: None,
            scale: 1.,
            focused: false,
        }
    }
}
#[derive(Debug, Clone, PartialEq)]
pub enum Snapshot {
    Media(Media),
    Navigation(Navigation),
    Cursor(String),
    Scroll(Scroll),
    Window(Window),
    Presentation(Presentation),
    Withdraw(Domain),
}
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Domain {
    Media = 1,
    Navigation = 2,
    Cursor = 3,
    Scroll = 4,
    Window = 5,
    Presentation = 6,
}
impl Domain {
    pub fn name(self) -> &'static str {
        match self {
            Self::Media => "media",
            Self::Navigation => "navigation",
            Self::Cursor => "cursor",
            Self::Scroll => "scroll",
            Self::Window => "window",
            Self::Presentation => "presentation",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "media" => Self::Media,
            "navigation" => Self::Navigation,
            "cursor" => Self::Cursor,
            "scroll" => Self::Scroll,
            "window" => Self::Window,
            "presentation" => Self::Presentation,
            _ => return None,
        })
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct Verb {
    pub domain: Domain,
    pub name: String,
    pub body: Value,
}
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Snapshot(Snapshot),
    Verb(Verb),
    Closed,
}
impl Snapshot {
    pub fn domain(&self) -> Domain {
        match self {
            Self::Media(_) => Domain::Media,
            Self::Navigation(_) => Domain::Navigation,
            Self::Cursor(_) => Domain::Cursor,
            Self::Scroll(_) => Domain::Scroll,
            Self::Window(_) => Domain::Window,
            Self::Presentation(_) => Domain::Presentation,
            Self::Withdraw(d) => *d,
        }
    }
    fn body(&self) -> Value {
        match self {
            Self::Media(v) => json!(v),
            Self::Navigation(v) => json!(v),
            Self::Cursor(v) => json!({"shape":v}),
            Self::Scroll(v) => json!(v),
            Self::Window(v) => json!(v),
            Self::Presentation(v) => json!(v),
            Self::Withdraw(_) => Value::Null,
        }
    }
    fn wire(&self) -> Value {
        json!({"version":1,"domain":self.domain().name(),"domain_version":1,"kind":"snapshot","body":self.body()})
    }
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "malformed affordances message")
}
fn number(v: &Value) -> io::Result<f64> {
    v.as_f64().filter(|v| v.is_finite()).ok_or_else(invalid)
}
fn nonnegative(v: &Value) -> io::Result<()> {
    if number(v)? < 0. { Err(invalid()) } else { Ok(()) }
}
fn size(v: &Value) -> io::Result<()> {
    if v.is_null() {
        return Ok(());
    }
    if number(&v["width"])? <= 0. || number(&v["height"])? <= 0. {
        return Err(invalid());
    }
    Ok(())
}
fn parse_snapshot(d: Domain, mut b: Value) -> io::Result<Snapshot> {
    if b.is_null() {
        return Ok(Snapshot::Withdraw(d));
    }
    // Nullable fields are required: serde's Option alone accepts missing fields.
    let required: &[&str] = match d {
        Domain::Media => &["status", "position", "rate", "duration", "title", "artwork", "capabilities"],
        Domain::Navigation => &["url", "title", "can_go_back", "can_go_forward", "loading", "capabilities"],
        Domain::Cursor => &["shape"],
        Domain::Scroll => &["x", "y", "capabilities"],
        Domain::Window => &["title", "requested_size", "ready"],
        Domain::Presentation => &["visible", "preferred_size", "scale", "focused"],
    };
    if !b.is_object() || required.iter().any(|k| b.get(k).is_none()) {
        return Err(invalid());
    }
    if matches!(d, Domain::Media | Domain::Navigation | Domain::Scroll) {
        let capabilities = b["capabilities"].as_object_mut().ok_or_else(invalid)?;
        capabilities.retain(|name, _| known_verb(d, name));
    }
    match d {
        Domain::Media => {
            for k in ["position", "duration"] {
                if !b[k].is_null() {
                    nonnegative(&b[k])?
                }
            }
            number(&b["rate"])?;
            if !b["position"].is_null() && !b["duration"].is_null() && number(&b["position"])? > number(&b["duration"])? {
                return Err(invalid());
            }
            let status = b["status"].as_str().ok_or_else(invalid)?;
            if !["playing", "paused", "stopped", "buffering", "unknown"].contains(&status) {
                b["status"] = json!("unknown")
            }
            if !b["artwork"].is_null() {
                let kind = b["artwork"]["kind"].as_str().ok_or_else(invalid)?;
                if !["url", "icon"].contains(&kind) {
                    b["artwork"] = Value::Null
                }
            }
            Ok(Snapshot::Media(serde_json::from_value(b)?))
        }
        Domain::Navigation => Ok(Snapshot::Navigation(serde_json::from_value(b)?)),
        Domain::Cursor => {
            let shape = b["shape"].as_str().ok_or_else(invalid)?;
            Ok(Snapshot::Cursor(if CURSORS.contains(&shape) { shape } else { "default" }.into()))
        }
        Domain::Scroll => {
            for k in ["x", "y"] {
                let a = &b[k];
                nonnegative(&a["content_length"])?;
                nonnegative(&a["viewport_length"])?;
                nonnegative(&a["position"])?;
                let p = number(&a["position"])?;
                if p > (number(&a["content_length"])? - number(&a["viewport_length"])?).max(0.) || (a["scrollable"] == false && p != 0.) {
                    return Err(invalid());
                }
            }
            Ok(Snapshot::Scroll(serde_json::from_value(b)?))
        }
        Domain::Window => {
            size(&b["requested_size"])?;
            Ok(Snapshot::Window(serde_json::from_value(b)?))
        }
        Domain::Presentation => {
            size(&b["preferred_size"])?;
            if number(&b["scale"])? <= 0. {
                return Err(invalid());
            }
            Ok(Snapshot::Presentation(serde_json::from_value(b)?))
        }
    }
}
pub const CURSORS: &[&str] = &[
    "auto",
    "default",
    "none",
    "context-menu",
    "help",
    "pointer",
    "progress",
    "wait",
    "cell",
    "crosshair",
    "text",
    "vertical-text",
    "alias",
    "copy",
    "move",
    "no-drop",
    "not-allowed",
    "grab",
    "grabbing",
    "e-resize",
    "n-resize",
    "ne-resize",
    "nw-resize",
    "s-resize",
    "se-resize",
    "sw-resize",
    "w-resize",
    "ew-resize",
    "ns-resize",
    "nesw-resize",
    "nwse-resize",
    "col-resize",
    "row-resize",
    "all-scroll",
    "zoom-in",
    "zoom-out",
];
fn known_verb(d: Domain, name: &str) -> bool {
    match d {
        Domain::Media => ["play", "pause", "stop", "next", "previous", "seek_absolute", "seek_relative"].contains(&name),
        Domain::Navigation => ["back", "forward", "reload", "stop", "load"].contains(&name),
        Domain::Scroll => ["scroll_by_step", "set_position"].contains(&name),
        _ => false,
    }
}
fn validate_verb(v: &Verb) -> io::Result<bool> {
    if !v.body.is_object() {
        return Err(invalid());
    }
    match (v.domain, v.name.as_str()) {
        (Domain::Media, "seek_absolute") => nonnegative(&v.body["position"])?,
        (Domain::Media, "seek_relative") => {
            number(&v.body["offset"])?;
        }
        (Domain::Navigation, "load") => {
            v.body["url"].as_str().ok_or_else(invalid)?;
        }
        (Domain::Scroll, name) => {
            let axis = v.body["axis"].as_str().ok_or_else(invalid)?;
            if !["x", "y"].contains(&axis) {
                return Ok(false);
            }
            if name == "set_position" {
                number(&v.body["position"])?;
            } else {
                let step = v.body["step"].as_str().ok_or_else(invalid)?;
                let direction = v.body["direction"].as_str().ok_or_else(invalid)?;
                if !["small", "large"].contains(&step) || !["decrement", "increment"].contains(&direction) {
                    return Ok(false);
                }
            }
        }
        _ => {}
    }
    Ok(true)
}
struct State {
    outgoing: VecDeque<Value>,
    bytes: usize,
    wire_bytes: usize,
    events: VecDeque<Event>,
    event_bytes: usize,
    snapshots: BTreeMap<Domain, Snapshot>,
    alive: bool,
}
struct Channel {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    producer: bool,
}
impl Channel {
    fn start(stream: Stream, producer: bool) -> io::Result<Self> {
        let wire = Framed::new(stream)?;
        let state = Arc::new(Mutex::new(State {
            outgoing: VecDeque::new(),
            bytes: 0,
            wire_bytes: 0,
            events: VecDeque::new(),
            event_bytes: 0,
            snapshots: BTreeMap::new(),
            alive: true,
        }));
        let shared = state.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let worker = thread::Builder::new().name("jackstay-affordances".into()).spawn(move || {
            let _ = drive(wire, &shared, &flag, producer);
            let mut s = shared.lock().unwrap();
            s.alive = false;
            s.snapshots.clear();
            s.events.clear();
            s.event_bytes = 0;
            s.events.push_back(Event::Closed);
        })?;
        Ok(Self {
            state,
            stop,
            worker: Some(worker),
            producer,
        })
    }
    fn enqueue(&self, v: Value) -> io::Result<()> {
        let mut s = self.state.lock().unwrap();
        self.enqueue_locked(&mut s, v)
    }
    fn enqueue_locked(&self, s: &mut State, v: Value) -> io::Result<()> {
        let n = serde_json::to_vec(&v)?.len() + 4;
        if !s.alive || self.stop.load(Ordering::Acquire) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if n > 131076 || s.bytes + s.wire_bytes + n > 524288 {
            self.stop.store(true, Ordering::Release);
            return Err(io::Error::other("affordances queue overflow"));
        }
        s.bytes += n;
        s.outgoing.push_back(v);
        Ok(())
    }
    fn publish(&self, v: Snapshot) -> io::Result<()> {
        if (v.domain() == Domain::Presentation) == self.producer {
            return Err(invalid());
        }
        if let Snapshot::Media(m) = &v {
            if m.position.is_some_and(|n| !n.is_finite()) || m.duration.is_some_and(|n| !n.is_finite()) {
                return Err(invalid());
            }
        }
        let v = parse_snapshot(v.domain(), v.body())?;
        let mut s = self.state.lock().unwrap();
        self.enqueue_locked(&mut s, v.wire())?;
        if matches!(v, Snapshot::Withdraw(_)) {
            s.snapshots.remove(&v.domain());
        } else {
            s.snapshots.insert(v.domain(), v);
        }
        Ok(())
    }
    fn poll(&self) -> Option<Event> {
        let mut s = self.state.lock().unwrap();
        let e = s.events.pop_front()?;
        s.event_bytes = s.event_bytes.saturating_sub(event_size(&e));
        Some(e)
    }
    fn close(&self) {
        self.stop.store(true, Ordering::Release)
    }
}
impl Drop for Channel {
    fn drop(&mut self) {
        self.close();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
fn event_size(e: &Event) -> usize {
    match e {
        Event::Snapshot(v) => serde_json::to_vec(&v.wire()).unwrap().len() + 4,
        Event::Verb(v) => serde_json::to_vec(&v.body).unwrap().len() + v.name.len() + 64,
        Event::Closed => 0,
    }
}
fn decode(v: Value, s: &State, producer: bool) -> io::Result<Option<Event>> {
    let o = v.as_object().ok_or_else(invalid)?;
    let version = o.get("version").and_then(Value::as_u64).ok_or_else(invalid)?;
    let d = o.get("domain").and_then(Value::as_str).ok_or_else(invalid)?;
    let dv = o.get("domain_version").and_then(Value::as_u64).ok_or_else(invalid)?;
    let kind = o.get("kind").and_then(Value::as_str).ok_or_else(invalid)?;
    if version != 1 {
        return Err(invalid());
    }
    let Some(d) = Domain::parse(d) else { return Ok(None) };
    if dv != 1 {
        return Ok(None);
    };
    match kind {
        "snapshot" => {
            if (d == Domain::Presentation) != producer {
                return Err(invalid());
            }
            Ok(Some(Event::Snapshot(parse_snapshot(
                d,
                o.get("body").ok_or_else(invalid)?.clone(),
            )?)))
        }
        "verb" => {
            if !producer || !matches!(d, Domain::Media | Domain::Navigation | Domain::Scroll) {
                return Err(invalid());
            }
            let name = o.get("verb").and_then(Value::as_str).ok_or_else(invalid)?;
            // Gate before body validation: unsupported and withdrawn verbs are ignored.
            if !known_verb(d, name) {
                return Ok(None);
            }
            let Some(snapshot) = s.snapshots.get(&d) else { return Ok(None) };
            if snapshot.body()["capabilities"][name] != true {
                return Ok(None);
            }
            let verb = Verb {
                domain: d,
                name: name.into(),
                body: o.get("body").ok_or_else(invalid)?.clone(),
            };
            if !validate_verb(&verb)? {
                return Ok(None);
            }
            if let Snapshot::Scroll(scroll) = snapshot {
                if (verb.body["axis"] == "x" && !scroll.x.scrollable) || (verb.body["axis"] == "y" && !scroll.y.scrollable) {
                    return Ok(None);
                }
            }
            Ok(Some(Event::Verb(verb)))
        }
        _ => Ok(None),
    }
}
fn drive(mut wire: Framed, state: &Mutex<State>, stop: &AtomicBool, producer: bool) -> io::Result<()> {
    while !stop.load(Ordering::Acquire) {
        {
            let mut s = state.lock().unwrap();
            for _ in 0..8 {
                let Some(v) = s.outgoing.pop_front() else { break };
                s.bytes -= serde_json::to_vec(&v)?.len() + 4;
                wire.send(v)?;
            }
            s.wire_bytes = wire.queued;
        }
        wire.flush()?;
        state.lock().unwrap().wire_bytes = wire.queued;
        for _ in 0..32 {
            let Some(v) = wire.receive::<Value>()? else { break };
            let mut s = state.lock().unwrap();
            if let Some(e) = decode(v, &s, producer)? {
                let bytes = event_size(&e);
                if s.event_bytes + bytes > 524288 || s.events.len() >= 1024 {
                    return Err(io::Error::other("affordances event overflow"));
                }
                if let Event::Snapshot(snapshot) = &e {
                    if matches!(snapshot, Snapshot::Withdraw(_)) {
                        s.snapshots.remove(&snapshot.domain());
                    } else {
                        s.snapshots.insert(snapshot.domain(), snapshot.clone());
                    }
                }
                s.event_bytes += bytes;
                s.events.push_back(e);
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}
/// Producer channel owner; drop joins its worker. No input cleanup is implied.
pub struct Producer(Channel);
impl Producer {
    pub fn start(stream: Stream) -> io::Result<Self> {
        Channel::start(stream, true).map(Self)
    }
    pub fn publish(&self, v: Snapshot) -> io::Result<()> {
        self.0.publish(v)
    }
    pub fn poll(&self) -> Option<Event> {
        self.0.poll()
    }
    pub fn close(&self) {
        self.0.close()
    }
    pub fn finished(&self) -> bool {
        !self.0.state.lock().unwrap().alive
    }
}
/// Host channel owner. Send success is enqueue success, never execution proof.
pub struct Host(Channel);
impl Host {
    pub fn start(stream: Stream) -> io::Result<Self> {
        Channel::start(stream, false).map(Self)
    }
    pub fn publish(&self, v: Presentation) -> io::Result<()> {
        self.0.publish(Snapshot::Presentation(v))
    }
    pub fn withdraw(&self) -> io::Result<()> {
        self.0.publish(Snapshot::Withdraw(Domain::Presentation))
    }
    pub fn send(&self, v: Verb) -> io::Result<()> {
        if !known_verb(v.domain, &v.name) || !validate_verb(&v)? {
            return Err(invalid());
        }
        self.0
            .enqueue(json!({"version":1,"domain":v.domain.name(),"domain_version":1,"kind":"verb","verb":v.name,"body":v.body}))
    }
    pub fn poll(&self) -> Option<Event> {
        self.0.poll()
    }
    pub fn close(&self) {
        self.0.close()
    }
    pub fn finished(&self) -> bool {
        !self.0.state.lock().unwrap().alive
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> State {
        State {
            outgoing: VecDeque::new(),
            bytes: 0,
            wire_bytes: 0,
            events: VecDeque::new(),
            event_bytes: 0,
            snapshots: BTreeMap::new(),
            alive: true,
        }
    }
    fn media() -> Snapshot {
        Snapshot::Media(Media {
            status: "paused".into(),
            position: Some(0.),
            rate: 0.,
            duration: Some(100.),
            title: None,
            artwork: None,
            capabilities: BTreeMap::from([("seek_absolute".into(), true), ("seek_relative".into(), true)]),
        })
    }
    // ADR v1: unsupported/withdrawn verbs are ignored before body validation;
    // enabled malformed verbs close. Enumerate missing, null, scalar, wrong type.
    #[test]
    fn capability_gate_precedes_body_validation() {
        for body in [
            None,
            Some(Value::Null),
            Some(json!(3)),
            Some(json!({"position":"bad"})),
            Some(json!({"position":-1})),
        ] {
            let mut s = state();
            let mut v = json!({"version":1,"domain_version":1,"domain":"media","kind":"verb","verb":"seek_absolute"});
            if let Some(b) = body {
                v["body"] = b
            }
            assert_eq!(decode(v.clone(), &s, true).unwrap(), None);
            s.snapshots.insert(Domain::Media, media());
            assert!(decode(v.clone(), &s, true).is_err());
            if let Snapshot::Media(m) = s.snapshots.get_mut(&Domain::Media).unwrap() {
                m.capabilities.clear()
            }
            assert_eq!(decode(v, &s, true).unwrap(), None);
        }
    }
    // #56: absolute seek is nonnegative; relative seek and scroll positions are
    // signed. Generate zero, negative zero, positive/negative and f64 boundaries.
    #[test]
    fn numeric_verb_boundaries() {
        for n in [-f64::MAX, -1., -0., 0., 1., f64::MAX] {
            let absolute = Verb {
                domain: Domain::Media,
                name: "seek_absolute".into(),
                body: json!({"position":n}),
            };
            assert_eq!(validate_verb(&absolute).is_ok(), n >= 0.);
            assert!(
                validate_verb(&Verb {
                    domain: Domain::Media,
                    name: "seek_relative".into(),
                    body: json!({"offset":n})
                })
                .unwrap()
            );
            assert!(
                validate_verb(&Verb {
                    domain: Domain::Scroll,
                    name: "set_position".into(),
                    body: json!({"axis":"x","position":n})
                })
                .unwrap()
            );
        }
    }
    // Complete snapshots validate nullable presence, ranges, enum fallbacks and
    // known message direction. Unknown domains/kinds/verbs stay ignorable.
    #[test]
    fn snapshot_validation_and_extensions() {
        let mut b = media().body();
        b.as_object_mut().unwrap().remove("position");
        assert!(parse_snapshot(Domain::Media, b).is_err());
        for n in [-1., 101.] {
            let mut b = media().body();
            b["position"] = json!(n);
            assert!(parse_snapshot(Domain::Media, b).is_err());
        }
        let mut b = media().body();
        b["status"] = json!("x-future");
        b["artwork"] = json!({"kind":"x-future"});
        let Snapshot::Media(m) = parse_snapshot(Domain::Media, b).unwrap() else {
            panic!()
        };
        assert_eq!(m.status, "unknown");
        assert_eq!(m.artwork, None);
        assert_eq!(
            parse_snapshot(Domain::Cursor, json!({"shape":"x-vendor"})).unwrap(),
            Snapshot::Cursor("default".into())
        );
        for scale in [-1., -0., 0.] {
            assert!(
                parse_snapshot(
                    Domain::Presentation,
                    json!({"visible":true,"focused":false,"scale":scale,"preferred_size":null})
                )
                .is_err()
            );
        }
        assert!(decode(media().wire(), &state(), true).is_err());
        assert!(
            decode(
                json!({"version":1,"domain_version":1,"domain":"x-vendor","kind":"verb"}),
                &state(),
                true
            )
            .unwrap()
            .is_none()
        );
    }
    #[cfg(unix)]
    fn pair() -> (Stream, Stream) {
        Stream::pair().unwrap()
    }
    #[cfg(windows)]
    fn pair() -> (Stream, Stream) {
        crate::local::pipe_pair().unwrap()
    }
    fn wait(mut f: impl FnMut() -> Option<Event>) -> Event {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(e) = f() {
                return e;
            }
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
    }
    // Real channel workers preserve snapshot/verb stream order, publish fresh
    // state on each connection, and make disconnect/overflow visible.
    #[test]
    fn channel_roundtrip_and_closure() {
        let (a, b) = pair();
        let p = Producer::start(a).unwrap();
        let h = Host::start(b).unwrap();
        p.publish(media()).unwrap();
        assert_eq!(wait(|| h.poll()), Event::Snapshot(media()));
        h.send(Verb {
            domain: Domain::Media,
            name: "seek_relative".into(),
            body: json!({"offset":-4}),
        })
        .unwrap();
        assert!(matches!(wait(|| p.poll()), Event::Verb(_)));
        h.publish(Presentation::default()).unwrap();
        assert_eq!(wait(|| p.poll()), Event::Snapshot(Snapshot::Presentation(Presentation::default())));
        drop(h);
        assert_eq!(wait(|| p.poll()), Event::Closed);
        assert!(p.publish(media()).is_err());
    }
    // The bound includes prefixes; a single oversized snapshot visibly closes.
    #[test]
    fn output_overflow_closes_channel() {
        let (a, _b) = pair();
        let p = Producer::start(a).unwrap();
        let mut m = match media() {
            Snapshot::Media(m) => m,
            _ => unreachable!(),
        };
        m.title = Some("x".repeat(131072));
        assert!(p.publish(Snapshot::Media(m)).is_err());
        assert_eq!(wait(|| p.poll()), Event::Closed);
    }
}
