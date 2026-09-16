use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::Instant,
};

use super::*;

#[derive(Clone)]
pub struct Target(Arc<Mutex<State>>);
pub struct Controller {
    target: Target,
    id: u64,
    mode: Mode,
    mailbox: Arc<Mutex<VecDeque<Status>>>,
}
struct Active {
    id: u64,
    mode: Mode,
    epoch: u64,
    last_sequence: u64,
    last_seen: Instant,
    keys: HashMap<u64, Key>,
    buttons: HashSet<u32>,
    mailbox: Arc<Mutex<VecDeque<Status>>>,
}
#[derive(Clone, Copy)]
struct Barrier {
    scope: Scope,
    reason: Reason,
    end: bool,
}
struct Flight {
    work: Work,
    end: bool,
}
struct State {
    config: Config,
    next_controller: u64,
    next_work: u64,
    active: Option<Active>,
    queue: VecDeque<Work>,
    flight: Option<Flight>,
    barrier: Option<Barrier>,
    failed: bool,
}
impl Target {
    pub fn new(config: Config) -> Result<Self, Error> {
        if config.modes == 0
            || config.modes & !7 != 0
            || config.capabilities & !CAP_ALL != 0
            || config.max_events == 0
            || config.max_events > 65536
            || config.max_bytes < 96
            || config.max_bytes > 16 * 1024 * 1024
            || config.max_text_bytes == 0
            || config.max_text_bytes > 16384
            || config.idle_timeout.as_millis() < 100
            || config.idle_timeout.as_secs() > 3600
            || !config.geometry.valid()
        {
            return Err(Error::Invalid);
        }
        Ok(Self(Arc::new(Mutex::new(State {
            config,
            next_controller: 0,
            next_work: 0,
            active: None,
            queue: VecDeque::new(),
            flight: None,
            barrier: None,
            failed: false,
        }))))
    }
    pub fn config(&self) -> Config {
        self.0.lock().unwrap().config.clone()
    }
    pub fn admit(&self, mode: Mode) -> Result<Controller, Error> {
        let mut s = self.0.lock().unwrap();
        if s.failed {
            return Err(Error::CleanupFailed);
        }
        if s.active.is_some() {
            return Err(Error::Busy);
        }
        if s.config.modes & mode.bit() == 0 {
            return Err(Error::Unsupported);
        }
        s.next_controller = s.next_controller.checked_add(1).ok_or(Error::Closed)?;
        let id = s.next_controller;
        let mailbox = Arc::new(Mutex::new(VecDeque::new()));
        s.active = Some(Active {
            id,
            mode,
            epoch: 1,
            last_sequence: 0,
            last_seen: Instant::now(),
            keys: HashMap::new(),
            buttons: HashSet::new(),
            mailbox: mailbox.clone(),
        });
        Ok(Controller {
            target: self.clone(),
            id,
            mode,
            mailbox,
        })
    }
    /// Poll work on the executor's thread. Only one work item may be in flight.
    pub fn next(&self) -> Option<Work> {
        let mut s = self.0.lock().unwrap();
        s.expire();
        if s.flight.is_some() {
            return None;
        }
        let (work, end) = if let Some(b) = s.barrier.take() {
            let a = s.active.as_ref()?;
            let (controller, epoch, mode) = (a.id, a.epoch, a.mode);
            s.next_work += 1;
            (
                Work {
                    mode,
                    id: s.next_work,
                    controller,
                    epoch,
                    sequence: 0,
                    operation: Operation::Cleanup {
                        scope: b.scope,
                        reason: b.reason,
                    },
                },
                b.end,
            )
        } else {
            loop {
                let mut work = s.queue.pop_front()?;
                match s.bind(&mut work) {
                    Ok(true) => break (work, false),
                    Ok(false) => {
                        s.status(Status::Completed {
                            sequence: work.sequence,
                            outcome: Outcome::Executed,
                        });
                        if s.barrier.is_some() {
                            return None;
                        }
                    }
                    Err(error) => {
                        s.status(Status::Rejected {
                            sequence: work.sequence,
                            error,
                        });
                        if s.barrier.is_some() {
                            return None;
                        }
                    }
                }
            }
        };
        s.flight = Some(Flight { work: work.clone(), end });
        Some(work)
    }
    pub fn complete(&self, id: u64, outcome: Outcome) -> Result<(), Error> {
        let mut s = self.0.lock().unwrap();
        if s.flight.as_ref().is_none_or(|f| f.work.id != id) {
            return Err(Error::Invalid);
        }
        let f = s.flight.take().unwrap();
        match f.work.operation {
            Operation::Event(event) => {
                if outcome == Outcome::Executed {
                    s.apply(&event);
                }
                s.status(Status::Completed {
                    sequence: f.work.sequence,
                    outcome,
                });
                if matches!(outcome, Outcome::Partial | Outcome::Uncertain) {
                    s.cancel(Scope::All, Reason::Execution, true);
                }
            }
            Operation::Cleanup { scope, reason } => {
                if outcome != Outcome::Executed {
                    s.failed = true;
                    s.queue.clear();
                    s.barrier = None;
                    s.status(Status::Closed { reason, clean: false });
                    s.active = None;
                    return Err(Error::CleanupFailed);
                }
                if let Some(a) = s.active.as_mut() {
                    a.buttons.clear();
                    if scope == Scope::All {
                        a.keys.clear();
                    }
                }
                if f.end {
                    s.status(Status::Closed { reason, clean: true });
                    s.active = None;
                    s.barrier = None;
                    s.queue.clear();
                } else if s.barrier.is_none() {
                    let geometry = s.config.geometry;
                    if let Some(a) = s.active.as_mut() {
                        a.epoch += 1;
                        let epoch = a.epoch;
                        // Preserved keyboard work belongs to the new interaction epoch.
                        for w in &mut s.queue {
                            w.epoch = epoch;
                        }
                        s.status(Status::Reset { epoch, geometry });
                    }
                }
            }
        }
        Ok(())
    }
    pub fn set_geometry(&self, geometry: Geometry) -> Result<(), Error> {
        let mut s = self.0.lock().unwrap();
        if !geometry.valid() || geometry.revision <= s.config.geometry.revision {
            return Err(Error::Invalid);
        }
        s.config.geometry = geometry;
        if s.active.is_some() {
            s.cancel(Scope::Pointer, Reason::Geometry, false);
        }
        Ok(())
    }
    /// Drive expiry even when the executor has no work. Transport workers do this.
    pub fn tick(&self) {
        self.0.lock().unwrap().expire();
    }
    pub fn failed(&self) -> bool {
        self.0.lock().unwrap().failed
    }
    pub fn idle(&self) -> bool {
        let s = self.0.lock().unwrap();
        s.active.is_none() && s.flight.is_none() && !s.failed
    }
    /// Host assertion that a failed executor has been rebuilt or otherwise resolved.
    pub fn resolve_failed_cleanup(&self) -> Result<(), Error> {
        let mut s = self.0.lock().unwrap();
        if !s.failed || s.flight.is_some() {
            return Err(Error::Invalid);
        }
        s.failed = false;
        Ok(())
    }
}
impl Controller {
    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn epoch(&self) -> Result<u64, Error> {
        let s = self.target.0.lock().unwrap();
        Ok(s.active.as_ref().filter(|a| a.id == self.id).ok_or(Error::Closed)?.epoch)
    }
    pub fn heartbeat(&self) -> Result<(), Error> {
        let mut s = self.target.0.lock().unwrap();
        s.expire();
        let a = s.active.as_mut().filter(|a| a.id == self.id).ok_or(Error::Closed)?;
        a.last_seen = Instant::now();
        Ok(())
    }
    /// Admission to the bounded queue, not executor completion. Sequence is strictly
    /// increasing across every attempted submission in this controller incarnation.
    pub fn submit(&self, epoch: u64, sequence: u64, event: Event) -> Result<(), Error> {
        let mut s = self.target.0.lock().unwrap();
        s.expire();
        let a = s.active.as_mut().filter(|a| a.id == self.id).ok_or(Error::Closed)?;
        if sequence == 0 || sequence <= a.last_sequence {
            return Err(Error::Invalid);
        }
        a.last_sequence = sequence;
        if epoch != a.epoch {
            return Err(Error::Stale);
        }
        if s.barrier.is_some()
            || s.flight
                .as_ref()
                .is_some_and(|f| matches!(f.work.operation, Operation::Cleanup { .. }))
        {
            return Err(Error::Busy);
        }
        validate(&s.config, s.active.as_ref().unwrap().mode, &event)?;
        let bytes: usize = s.queue.iter().map(work_bytes).sum::<usize>() + s.flight.as_ref().map_or(0, |f| work_bytes(&f.work));
        if s.queue.len() + usize::from(s.flight.is_some()) >= s.config.max_events || bytes + event.bytes() > s.config.max_bytes {
            s.cancel(Scope::All, Reason::Overflow, true);
            return Err(Error::Overflow);
        }
        s.next_work += 1;
        let id = s.next_work;
        s.queue.push_back(Work {
            mode: self.mode,
            id,
            controller: self.id,
            epoch,
            sequence,
            operation: Operation::Event(event),
        });
        Ok(())
    }
    pub fn reset(&self) -> Result<(), Error> {
        let mut s = self.target.0.lock().unwrap();
        if s.active.as_ref().is_none_or(|a| a.id != self.id) {
            return Err(Error::Closed);
        }
        s.cancel(Scope::All, Reason::Focus, false);
        Ok(())
    }
    pub fn close(&self) {
        let mut s = self.target.0.lock().unwrap();
        if s.active.as_ref().is_some_and(|a| a.id == self.id) {
            s.cancel(Scope::All, Reason::Disconnect, true);
        }
    }
    pub fn poll(&self) -> Option<Status> {
        self.mailbox.lock().unwrap().pop_front()
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        let mut s = self.target.0.lock().unwrap();
        if s.active.as_ref().is_some_and(|a| a.id == self.id) {
            s.cancel(Scope::All, Reason::Disconnect, true);
        }
    }
}
impl State {
    fn expire(&mut self) {
        if self
            .active
            .as_ref()
            .is_some_and(|a| a.last_seen.elapsed() >= self.config.idle_timeout)
        {
            self.cancel(Scope::All, Reason::Expired, true);
        }
    }
    fn status(&mut self, status: Status) {
        let Some(a) = &self.active else {
            return;
        };
        let mut mailbox = a.mailbox.lock().unwrap();
        if mailbox.len() >= self.config.max_events * 2 + 4 {
            mailbox.clear();
            mailbox.push_back(Status::Closed {
                reason: Reason::Overflow,
                clean: false,
            });
            drop(mailbox);
            self.queue.clear();
            self.barrier = Some(Barrier {
                scope: Scope::All,
                reason: Reason::Overflow,
                end: true,
            });
        } else {
            mailbox.push_back(status);
        }
    }
    fn cancel(&mut self, scope: Scope, reason: Reason, end: bool) {
        if self.active.is_none() {
            return;
        }
        let old = self.barrier;
        let scope = if old.is_some_and(|b| b.scope == Scope::All) || end {
            Scope::All
        } else {
            scope
        };
        let end = end || old.is_some_and(|b| b.end) || self.flight.as_ref().is_some_and(|f| f.end);
        let reason = old.filter(|b| b.end).map_or(reason, |b| b.reason);
        // Repeated expiry while cleanup is already in flight must not enqueue
        // unbounded barriers; the in-flight all-state cleanup already suffices.
        if self.flight.as_ref().is_some_and(|f| f.end) {
            return;
        }
        self.barrier = Some(Barrier { scope, reason, end });
        let mut keep = VecDeque::new();
        while let Some(w) = self.queue.pop_front() {
            if scope == Scope::All || matches!(&w.operation, Operation::Event(e) if e.pointer()) {
                self.status(Status::Rejected {
                    sequence: w.sequence,
                    error: Error::Stale,
                });
            } else {
                keep.push_back(w);
            }
        }
        self.queue = keep;
    }
    fn bind(&self, w: &mut Work) -> Result<bool, Error> {
        let a = self.active.as_ref().ok_or(Error::Closed)?;
        match &mut w.operation {
            Operation::Event(Event::Key { press, action, key, .. }) => match action {
                Action::Down => {
                    if a.keys.contains_key(press) || a.keys.len() >= self.config.max_events {
                        return Err(Error::Invalid);
                    }
                }
                Action::Up | Action::Repeat => {
                    let Some(binding) = a.keys.get(press) else {
                        return if *action == Action::Up { Ok(false) } else { Err(Error::Invalid) };
                    };
                    *key = binding.clone();
                }
            },
            Operation::Event(Event::Button {
                button,
                action: Action::Up,
                ..
            }) if !a.buttons.contains(button) => return Ok(false),
            Operation::Event(Event::Button { button, action, .. }) if (*action == Action::Down) == a.buttons.contains(button) => {
                return Err(Error::Invalid);
            }
            _ => {}
        }
        Ok(true)
    }
    fn apply(&mut self, event: &Event) {
        let Some(a) = self.active.as_mut() else {
            return;
        };
        match event {
            Event::Key {
                press,
                action: Action::Down,
                key,
                ..
            } => {
                a.keys.insert(*press, key.clone());
            }
            Event::Key {
                press, action: Action::Up, ..
            } => {
                a.keys.remove(press);
            }
            Event::Button {
                button,
                action: Action::Down,
                ..
            } => {
                a.buttons.insert(*button);
            }
            Event::Button {
                button,
                action: Action::Up,
                ..
            } => {
                a.buttons.remove(button);
            }
            _ => {}
        }
    }
}
fn work_bytes(w: &Work) -> usize {
    match &w.operation {
        Operation::Event(e) => e.bytes(),
        _ => 0,
    }
}
pub(super) fn validate(c: &Config, mode: Mode, e: &Event) -> Result<(), Error> {
    let capability = match e {
        Event::Key {
            press,
            action,
            key,
            modifiers,
        } => {
            if *press == 0 || modifiers & !0xff != 0 {
                return Err(Error::Invalid);
            }
            if *action == Action::Repeat && mode == Mode::Physical {
                return Err(Error::Unsupported);
            }
            let (name, cap) = match key {
                Key::Physical(n) => (n, CAP_PHYSICAL),
                Key::Logical(n) => (n, CAP_LOGICAL),
            };
            if name.is_empty() || name.len() > 63 || name.contains('\0') {
                return Err(Error::Invalid);
            }
            if *action != Action::Down {
                return Ok(());
            }
            if matches!(key, Key::Physical(_)) && !name.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Err(Error::Invalid);
            }
            cap
        }
        Event::Text(t) => {
            if t.is_empty() || t.len() > c.max_text_bytes {
                return Err(Error::Invalid);
            }
            CAP_TEXT
        }
        Event::Motion(p) => {
            position(c, p)?;
            CAP_POINTER
        }
        Event::Button {
            button,
            action,
            position: p,
        } => {
            if !(1..=32).contains(button) || *action == Action::Repeat || !p.x.is_finite() || !p.y.is_finite() {
                return Err(Error::Invalid);
            }
            if *action == Action::Down {
                position(c, p)?;
            } // Release does not depend on current geometry.
            CAP_POINTER
        }
        Event::Scroll { x, y, position: p, .. } => {
            if !x.is_finite() || !y.is_finite() {
                return Err(Error::Invalid);
            }
            position(c, p)?;
            CAP_SCROLL
        }
    };
    if c.capabilities & capability == 0 {
        return Err(Error::Unsupported);
    }
    Ok(())
}
fn position(c: &Config, p: &Position) -> Result<(), Error> {
    if p.revision != c.geometry.revision {
        return Err(Error::Stale);
    }
    if !p.x.is_finite() || !p.y.is_finite() || p.x < 0.0 || p.y < 0.0 || p.x >= c.geometry.width || p.y >= c.geometry.height {
        return Err(Error::Invalid);
    }
    Ok(())
}
