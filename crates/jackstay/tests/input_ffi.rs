use std::ptr;

use jackstay::{ffi::*, ffi_input::*};
#[test]
fn c_input_layouts_and_recoverable_handle_destruction_match_header() {
    if usize::BITS == 64 {
        assert_eq!(size_of::<FtInputConfig>(), 56);
        assert_eq!(size_of::<FtInputEvent>(), 152);
        assert_eq!(std::mem::offset_of!(FtInputEvent, text), 136);
        assert_eq!(size_of::<FtInputOperation>(), 192);
        assert_eq!(size_of::<FtInputStatus>(), 56);
    }
    // SAFETY: live disjoint stack storage, unique null-initialized handle slots.
    unsafe {
        let mut config = FtInputConfig::default();
        ft_input_config_default(&mut config);
        let mut target = ptr::null_mut();
        assert_eq!(ft_input_target_create(&config, &mut target), FT_STATUS_OK);
        assert_eq!(ft_input_target_destroy(&mut target), FT_STATUS_OK);
        assert!(target.is_null());
        assert_eq!(ft_input_target_destroy(&mut target), FT_STATUS_OK);
        config.max_text_bytes = 16385;
        assert_eq!(ft_input_target_create(&config, &mut target), FT_STATUS_INVALID_ARGUMENT);
        assert!(target.is_null());
    }
}

#[test]
fn c_input_serves_and_connects_over_local_endpoint_connections() {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        thread,
        time::{Duration, Instant},
    };

    use jackstay::{
        ffi_local::FtLocalConnection,
        local::{self, Endpoint, Scope, Transport},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = format!("input-ffi-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
    let endpoint = Endpoint::new(Scope::User, &name, Transport::LocalStream).unwrap();
    let listener = local::Listener::bind(&endpoint).unwrap();
    let connecting = thread::spawn(move || local::connect(&endpoint).unwrap());
    let accepted: *mut FtLocalConnection = Box::into_raw(Box::new(listener.accept().unwrap().into()));
    let connected: *mut FtLocalConnection = Box::into_raw(Box::new(connecting.join().unwrap().into()));
    // SAFETY: exclusively owned handles, each consumed or destroyed once.
    unsafe {
        let mut config = FtInputConfig::default();
        ft_input_config_default(&mut config);
        let mut target = ptr::null_mut();
        assert_eq!(ft_input_target_create(&config, &mut target), FT_STATUS_OK);
        let (mut accepted, mut connected) = (accepted, connected);
        let mut server = ptr::null_mut();
        assert_eq!(ft_input_target_serve_local(target, &mut accepted, &mut server), FT_STATUS_OK);
        assert!(accepted.is_null());
        let mut client = ptr::null_mut();
        // An invalid mode is rejected before the connection is taken.
        assert_eq!(
            ft_input_client_connect_local(&mut connected, 3, &mut client),
            FT_STATUS_INVALID_ARGUMENT
        );
        assert!(!connected.is_null());
        let target_address = target as usize;
        let executor = thread::spawn(move || {
            let target = target_address as *mut FtInputTarget;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let mut work = ptr::null_mut();
                if ft_input_target_next(target, &mut work) == FT_STATUS_OK {
                    assert_eq!(ft_input_work_complete(&mut work, 0), FT_STATUS_OK);
                    return;
                }
                assert!(Instant::now() < deadline, "no input work");
                thread::sleep(Duration::from_millis(2));
            }
        });
        assert_eq!(ft_input_client_connect_local(&mut connected, 4, &mut client), FT_STATUS_OK);
        assert!(connected.is_null());
        let text = "local";
        let event = FtInputEvent {
            kind: 2,
            text: text.as_ptr(),
            text_len: text.len(),
            ..Default::default()
        };
        let mut sequence = 0;
        assert_eq!(ft_input_client_send(client, &event, &mut sequence), FT_STATUS_OK);
        executor.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut status = FtInputStatus::default();
        while ft_input_client_poll(client, &mut status) == FT_STATUS_EMPTY {
            assert!(Instant::now() < deadline, "no completion");
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!((status.kind, status.sequence, status.result), (1, sequence, 0));
        ft_input_client_destroy(&mut client);
        ft_input_server_destroy(&mut server);
        while ft_input_target_destroy(&mut target) != FT_STATUS_OK {
            let mut work = ptr::null_mut();
            if ft_input_target_next(target, &mut work) == FT_STATUS_OK {
                assert_eq!(ft_input_work_complete(&mut work, 0), FT_STATUS_OK);
            }
            assert!(Instant::now() < deadline, "cleanup did not settle");
            thread::sleep(Duration::from_millis(2));
        }
    }
}
