//! One launchd listener, with host-selected publications routed by capability.
use std::sync::{OnceLock, Weak};

use super::*;

struct Routes<T> {
    targets: HashMap<Option<String>, Arc<T>>,
    connections: HashMap<u64, (Option<String>, Weak<T>)>,
}

impl<T> Default for Routes<T> {
    fn default() -> Self {
        Self {
            targets: HashMap::new(),
            connections: HashMap::new(),
        }
    }
}

impl<T> Routes<T> {
    fn select(&mut self, identity: u64, token: Option<&str>) -> Result<Arc<T>, ArenaError> {
        if let Some((key, original)) = self.connections.get(&identity) {
            let target = original.upgrade().ok_or_else(|| failure("publication retired"))?;
            if !self.targets.get(key).is_some_and(|current| Arc::ptr_eq(current, &target)) {
                return Err(failure("publication retired"));
            }
            return Ok(target);
        }
        let key = token.map(str::to_owned);
        let (key, target) = self
            .targets
            .get_key_value(&key)
            .or_else(|| self.targets.get_key_value(&None))
            .ok_or_else(|| failure("authorization rejected"))?;
        let target = target.clone();
        self.connections.insert(identity, (key.clone(), Arc::downgrade(&target)));
        Ok(target)
    }
}

struct Listener(NonNull<c_void>);
// The Objective-C listener serializes its callbacks and supports invalidation
// from any thread. A Mutex around this owner makes the shared service Sync.
unsafe impl Send for Listener {}
impl Drop for Listener {
    fn drop(&mut self) {
        unsafe { ffi::jsa_server_stop(self.0.as_ptr()) };
    }
}
struct Service {
    _listener: Mutex<Listener>,
    routes: Arc<Mutex<Routes<ServerState>>>,
}
fn services() -> &'static Mutex<HashMap<String, Weak<Service>>> {
    static SERVICES: OnceLock<Mutex<HashMap<String, Weak<Service>>>> = OnceLock::new();
    SERVICES.get_or_init(Mutex::default)
}

pub(super) struct Registration {
    name: String,
    token: Option<String>,
    service: Option<Arc<Service>>,
}
impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registration").field("name", &self.name).finish_non_exhaustive()
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        // Serialize the last listener's destruction with creation of a new
        // listener for the same Mach service name.
        let mut services = services().lock().expect("named acquisition services");
        if let Some(service) = self.service.take() {
            {
                let mut routes = service.routes.lock().expect("named acquisition routes");
                if let Some(target) = routes.targets.remove(&self.token) {
                    target.close_all();
                }
            }
            drop(service);
            if services.get(&self.name).is_some_and(|service| service.strong_count() == 0) {
                services.remove(&self.name);
            }
        }
    }
}

pub(super) fn register(name: &str, token: Option<String>, producer: Producer) -> Result<Registration, ArenaError> {
    let c_name = CString::new(name).map_err(|_| failure("Mach service name contains NUL"))?;
    let mut services = services().lock().expect("named acquisition services");
    let service = match services.get(name).and_then(Weak::upgrade) {
        Some(service) => service,
        None => {
            let routes = Arc::new(Mutex::new(Routes::default()));
            let context = Box::into_raw(Box::new(routes.clone()));
            let raw = unsafe { ffi::jsa_server_start(c_name.as_ptr(), context.cast(), request, close, destroy) };
            let service = Arc::new(Service {
                _listener: Mutex::new(Listener(NonNull::new(raw).expect("named acquisition listener"))),
                routes,
            });
            services.insert(name.to_owned(), Arc::downgrade(&service));
            service
        }
    };
    {
        let mut routes = service.routes.lock().expect("named acquisition routes");
        if routes.targets.contains_key(&token) {
            return Err(failure("authorization token already registered on this service"));
        }
        routes.targets.insert(
            token.clone(),
            Arc::new(ServerState {
                token: token.clone(),
                producer,
                sessions: Mutex::new(HashMap::new()),
            }),
        );
    }
    Ok(Registration {
        name: name.to_owned(),
        token,
        service: Some(service),
    })
}

type Context = Arc<Mutex<Routes<ServerState>>>;
extern "C" fn request(context: *mut c_void, identity: u64, pid: u32, raw: *mut c_void) -> *mut c_void {
    let routes = unsafe { &*context.cast::<Context>() };
    let envelope = Envelope(NonNull::new(unsafe { ffi::jsa_object_retain(raw) }).expect("acquisition request"));
    let reply = envelope.decode::<Request>().and_then(|payload| {
        let mut routes = routes.lock().expect("named acquisition routes");
        let token = match &payload.metadata {
            Request::Authorize { token } => Some(token.as_str()),
            _ => None,
        };
        let target = routes.select(identity, token)?;
        // Hold the route lock through handling so retirement closes every
        // admitted incarnation and cannot race a late attach.
        target.handle(identity, pid, payload)
    });
    match reply {
        Ok(reply) => reply.into_raw(),
        Err(error) => Envelope::new(
            &Response::Error {
                message: error.to_string(),
            },
            &[],
            &[],
            None,
        )
        .map_or(std::ptr::null_mut(), Envelope::into_raw),
    }
}
extern "C" fn close(context: *mut c_void, identity: u64) {
    let routes = unsafe { &*context.cast::<Context>() };
    if let Some((_, target)) = routes.lock().expect("named acquisition routes").connections.remove(&identity) {
        if let Some(target) = target.upgrade() {
            target.close(identity);
        }
    }
}
extern "C" fn destroy(context: *mut c_void) {
    drop(unsafe { Box::from_raw(context.cast::<Context>()) });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publications_route_by_token_and_connections_cannot_switch_or_rebind() {
        let mut routes = Routes::default();
        routes.targets.insert(Some("a".into()), Arc::new(1));
        routes.targets.insert(Some("b".into()), Arc::new(2));
        assert!(routes.select(1, None).is_err());
        assert!(routes.select(1, Some("wrong")).is_err());
        assert_eq!(*routes.select(1, Some("a")).unwrap(), 1);
        assert_eq!(*routes.select(2, Some("b")).unwrap(), 2);
        assert_eq!(*routes.select(1, Some("b")).unwrap(), 1);
        let retired = routes.targets.remove(&Some("a".into())).unwrap();
        assert!(routes.select(1, Some("b")).is_err());
        routes.targets.insert(Some("a".into()), Arc::new(3));
        assert!(routes.select(1, Some("a")).is_err());
        assert_eq!(*routes.select(3, Some("a")).unwrap(), 3);
        assert_eq!(*routes.select(2, None).unwrap(), 2);
        assert_eq!(*retired, 1);
    }
}
