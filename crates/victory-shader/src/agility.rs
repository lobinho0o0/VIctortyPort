//! The D3D12 Agility SDK in this crate's test executables.
//!
//! Windows 10's in-box D3D12 stops at shader model 6.5, so the tests that
//! run translated shaders on a host device found none there. D3D12 looks
//! for these two exports on the process's executable, as for `victory.exe`
//! (`apps/victory/src/agility.rs`); `build.rs` exports them only when the
//! runtime `cargo xtask agility` fetches is in `<target>/<profile>/D3D12/`.
//! Unexported, they are inert.
#![allow(non_upper_case_globals)]

/// The runtime's SDK version: the minor number of the package
/// `cargo xtask agility` fetches. Keep it in step with `victory.exe`'s.
#[unsafe(no_mangle)]
pub static D3D12SDKVersion: u32 = 620;

/// A C string pointer D3D12 reads once at device creation.
#[repr(transparent)]
pub struct SdkPath(*const std::ffi::c_char);

// SAFETY: points at a static, immutable C string.
unsafe impl Sync for SdkPath {}

/// Where `D3D12Core.dll` is, relative to the executable: test executables
/// are in `<profile>/deps/`, the runtime in `<profile>/D3D12/`.
#[unsafe(no_mangle)]
pub static D3D12SDKPath: SdkPath = SdkPath(c"..\\D3D12\\".as_ptr());
