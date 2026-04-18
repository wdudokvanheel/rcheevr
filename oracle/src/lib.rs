//! Dev-only FFI wrapper around rcheevos used as a differential oracle.
//!
//! Never shipped in production. Gated behind the `oracle` feature on the
//! `rveevr` crate. This crate is a separate workspace member so cargo can
//! skip compiling it (and the C sources) for normal builds.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]

use std::ffi::CString;
use std::os::raw::{c_int, c_void};

pub mod ffi {
    use super::*;

    pub type rc_peek_t =
        Option<unsafe extern "C" fn(address: u32, num_bytes: u32, ud: *mut c_void) -> u32>;

    #[repr(C)]
    pub struct rc_trigger_t {
        _opaque: [u8; 0],
    }

    #[repr(C)]
    pub struct rc_value_t {
        _opaque: [u8; 0],
    }

    // State constants (see rc_runtime_types.h).
    pub const RC_TRIGGER_STATE_INACTIVE: c_int = 0;
    pub const RC_TRIGGER_STATE_WAITING: c_int = 1;
    pub const RC_TRIGGER_STATE_ACTIVE: c_int = 2;
    pub const RC_TRIGGER_STATE_PAUSED: c_int = 3;
    pub const RC_TRIGGER_STATE_RESET: c_int = 4;
    pub const RC_TRIGGER_STATE_TRIGGERED: c_int = 5;
    pub const RC_TRIGGER_STATE_PRIMED: c_int = 6;
    pub const RC_TRIGGER_STATE_DISABLED: c_int = 7;

    extern "C" {
        pub fn rc_trigger_size(memaddr: *const i8) -> c_int;
        pub fn rc_parse_trigger(
            buffer: *mut c_void,
            memaddr: *const i8,
            unused_l: *mut c_void,
            unused_funcs_idx: c_int,
        ) -> *mut rc_trigger_t;
        pub fn rc_evaluate_trigger(
            trigger: *mut rc_trigger_t,
            peek: rc_peek_t,
            ud: *mut c_void,
            unused_l: *mut c_void,
        ) -> c_int;
        pub fn rc_reset_trigger(trigger: *mut rc_trigger_t);

        pub fn rc_value_size(memaddr: *const i8) -> c_int;
        pub fn rc_parse_value(
            buffer: *mut c_void,
            memaddr: *const i8,
            unused_l: *mut c_void,
            unused_funcs_idx: c_int,
        ) -> *mut rc_value_t;
        pub fn rc_evaluate_value(
            value: *mut rc_value_t,
            peek: rc_peek_t,
            ud: *mut c_void,
            unused_l: *mut c_void,
        ) -> i32;
        pub fn rc_reset_value(value: *mut rc_value_t);
    }
}

/// Owned trigger backed by an internal buffer rcheevos parses into.
///
/// The rcheevos parser writes the condition graph into a caller-provided
/// buffer. We allocate a Vec, hand rcheevos the pointer, and keep the Vec
/// alive for the lifetime of the `Trigger` so the graph pointers stay valid.
pub struct Trigger {
    buffer: Vec<u8>,
    handle: *mut ffi::rc_trigger_t,
    _memaddr: CString,
}

#[derive(Debug)]
pub enum OracleError {
    ParseSize(i32),
    ParseFailed(i32),
    InvalidCString,
}

impl Trigger {
    pub fn parse(memaddr: &str) -> Result<Self, OracleError> {
        let cstr = CString::new(memaddr).map_err(|_| OracleError::InvalidCString)?;
        let needed = unsafe { ffi::rc_trigger_size(cstr.as_ptr()) };
        if needed < 0 {
            return Err(OracleError::ParseSize(needed));
        }
        let size = needed as usize;
        // Allocate aligned storage. rcheevos aligns internally from the base pointer.
        let mut buffer: Vec<u8> = vec![0u8; size + 16];
        let handle = unsafe {
            ffi::rc_parse_trigger(
                buffer.as_mut_ptr() as *mut c_void,
                cstr.as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        };
        if handle.is_null() {
            return Err(OracleError::ParseFailed(-1));
        }
        Ok(Self {
            buffer,
            handle,
            _memaddr: cstr,
        })
    }

    /// Evaluate one frame with the given peek callback.
    /// Returns the raw trigger state (see `ffi::RC_TRIGGER_STATE_*`).
    pub fn evaluate<F: FnMut(u32, u32) -> u32>(&mut self, mut peek: F) -> i32 {
        // Thunk: rcheevos calls us with (address, num_bytes, ud). ud points
        // at a &mut dyn FnMut we package here.
        unsafe extern "C" fn thunk(address: u32, num_bytes: u32, ud: *mut c_void) -> u32 {
            let cb: &mut &mut dyn FnMut(u32, u32) -> u32 = &mut *(ud as *mut _);
            cb(address, num_bytes)
        }
        let mut boxed: &mut dyn FnMut(u32, u32) -> u32 = &mut peek;
        let ud: *mut c_void = &mut boxed as *mut _ as *mut c_void;
        unsafe { ffi::rc_evaluate_trigger(self.handle, Some(thunk), ud, std::ptr::null_mut()) }
    }

    pub fn is_triggered(&mut self) -> bool {
        // Peek at state field without re-running eval: rcheevos stores it in
        // the rc_trigger_t we already have. But our handle is opaque — expose
        // a helper that evaluates with a zero peek to report current state
        // would mutate hit counts, so we don't. Callers should use the return
        // of `evaluate` instead.
        false
    }

    pub fn reset(&mut self) {
        unsafe { ffi::rc_reset_trigger(self.handle) };
    }
}

/// Owned value expression backed by an internal buffer rcheevos parses into.
///
/// Used to evaluate leaderboard VALUE condsets. Same allocation pattern as
/// `Trigger`: allocate a Vec, parse into it, keep alive for the handle's
/// lifetime.
pub struct Value {
    buffer: Vec<u8>,
    handle: *mut ffi::rc_value_t,
    _memaddr: CString,
}

impl Value {
    pub fn parse(memaddr: &str) -> Result<Self, OracleError> {
        let cstr = CString::new(memaddr).map_err(|_| OracleError::InvalidCString)?;
        let needed = unsafe { ffi::rc_value_size(cstr.as_ptr()) };
        if needed < 0 {
            return Err(OracleError::ParseSize(needed));
        }
        let size = needed as usize;
        let mut buffer: Vec<u8> = vec![0u8; size + 16];
        let handle = unsafe {
            ffi::rc_parse_value(
                buffer.as_mut_ptr() as *mut c_void,
                cstr.as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        };
        if handle.is_null() {
            return Err(OracleError::ParseFailed(-1));
        }
        Ok(Self {
            buffer,
            handle,
            _memaddr: cstr,
        })
    }

    /// Evaluate one frame with the given peek callback.
    /// Returns the computed i32 value.
    pub fn evaluate<F: FnMut(u32, u32) -> u32>(&mut self, mut peek: F) -> i32 {
        unsafe extern "C" fn thunk(address: u32, num_bytes: u32, ud: *mut c_void) -> u32 {
            let cb: &mut &mut dyn FnMut(u32, u32) -> u32 = &mut *(ud as *mut _);
            cb(address, num_bytes)
        }
        let mut boxed: &mut dyn FnMut(u32, u32) -> u32 = &mut peek;
        let ud: *mut c_void = &mut boxed as *mut _ as *mut c_void;
        unsafe { ffi::rc_evaluate_value(self.handle, Some(thunk), ud, std::ptr::null_mut()) }
    }

    pub fn reset(&mut self) {
        unsafe { ffi::rc_reset_value(self.handle) };
    }
}

// The trigger is not Send/Sync by default; make that explicit. Callers run
// it on one thread at a time.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_trivial_trigger() {
        let t = Trigger::parse("0xH0000=1");
        assert!(t.is_ok(), "parse failed: {:?}", t.err());
    }

    #[test]
    fn evaluate_triggers_when_memory_matches() {
        let mut t = Trigger::parse("0xH0000=1").unwrap();
        let mut mem: Vec<u8> = vec![0; 16];
        // Frame 1: byte[0] = 0 — not triggered.
        let state = t.evaluate(|addr, _n| mem[addr as usize] as u32);
        assert_ne!(state, ffi::RC_TRIGGER_STATE_TRIGGERED);
        // Frame 2: byte[0] = 1 — should trigger.
        mem[0] = 1;
        let state = t.evaluate(|addr, _n| mem[addr as usize] as u32);
        assert_eq!(state, ffi::RC_TRIGGER_STATE_TRIGGERED);
    }

    #[test]
    fn parse_multi_condset() {
        let t = Trigger::parse("0xH0000=1_0xH0001=2S0xH0002=3");
        assert!(t.is_ok(), "parse failed: {:?}", t.err());
    }

    #[test]
    fn hit_count_requires_multiple_frames() {
        let mut t = Trigger::parse("0xH0000=1(3)").unwrap();
        let mem = vec![1u8; 4];
        for _ in 0..2 {
            let state = t.evaluate(|addr, _n| mem[addr as usize] as u32);
            assert_ne!(state, ffi::RC_TRIGGER_STATE_TRIGGERED);
        }
        let state = t.evaluate(|addr, _n| mem[addr as usize] as u32);
        assert_eq!(state, ffi::RC_TRIGGER_STATE_TRIGGERED);
    }

    #[test]
    fn parse_value_single_operand() {
        let v = Value::parse("0xH0000");
        assert!(v.is_ok(), "value parse failed: {:?}", v.err());
    }

    #[test]
    fn evaluate_value_single_byte() {
        let mut v = Value::parse("0xH0000").unwrap();
        let mem = vec![42u8; 4];
        let val = v.evaluate(|addr, _n| mem[addr as usize] as u32);
        assert_eq!(val, 42);
    }

    #[test]
    fn evaluate_value_add_source() {
        // rcheevos' rc_parse_value dispatches to rc_parse_cond_value when the
        // string starts with "X:" (flag prefix). That path requires a Measured
        // (M:) terminal condition — bare "A:0xH0000_0xH0001" fails with -6.
        let mut v = Value::parse("A:0xH0000_M:0xH0001").unwrap();
        let mem = vec![10u8, 20u8, 0, 0];
        let val = v.evaluate(|addr, _n| mem[addr as usize] as u32);
        assert_eq!(val, 30);
    }
}
