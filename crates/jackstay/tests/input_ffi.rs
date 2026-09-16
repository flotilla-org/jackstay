#![cfg(unix)]
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
