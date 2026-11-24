// A standalone PE with no CRT startup. VIctory calls its entry point on a
// title thread. Success requires a console import and a host import to work.
#![no_std]
#![no_main]

use core::panic::PanicInfo;

#[link(name = "xgameruntime", kind = "raw-dylib")]
unsafe extern "system" {
    fn XGameRuntimeInitialize() -> i32;
}

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    fn GetCurrentProcessId() -> u32;
}

#[unsafe(no_mangle)]
pub extern "system" fn title_main(_: *mut core::ffi::c_void) -> u32 {
    // SAFETY: these imports take no arguments.
    if unsafe { XGameRuntimeInitialize() == 0 && GetCurrentProcessId() != 0 } {
        42
    } else {
        1
    }
}

#[panic_handler]
fn panic(_: &PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
