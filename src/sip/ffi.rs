use std::ffi::{c_char, c_int, c_void};

pub const SOFIA_EV_REGISTER_OK: c_int = 1;
pub const SOFIA_EV_REGISTER_FAIL: c_int = 2;
pub const SOFIA_EV_INCOMING_CALL: c_int = 3;
pub const SOFIA_EV_CALL_CONNECTED: c_int = 4;
pub const SOFIA_EV_CALL_ENDED: c_int = 5;
pub const SOFIA_EV_CALL_FAILED: c_int = 6;
pub const SOFIA_EV_CALL_MEDIA: c_int = 7;
pub const SOFIA_EV_TRANSFER_OK: c_int = 8;
pub const SOFIA_EV_TRANSFER_FAILED: c_int = 9;
pub const SOFIA_EV_CONSULT_RINGING: c_int = 10;
pub const SOFIA_EV_CONSULT_CONNECTED: c_int = 11;
pub const SOFIA_EV_CONSULT_MEDIA: c_int = 12;
pub const SOFIA_EV_CONSULT_ENDED: c_int = 13;

pub type SofiaEventCb = unsafe extern "C" fn(
    event: c_int,
    status: c_int,
    phrase: *const c_char,
    aux: *const c_char,
    userdata: *mut c_void,
);

#[repr(C)]
pub struct SofiaCtx {
    _private: [u8; 0],
}

extern "C" {
    pub fn sofia_ctx_create(
        server: *const c_char,
        port: c_int,
        cb: SofiaEventCb,
        userdata: *mut c_void,
    ) -> *mut SofiaCtx;
    pub fn sofia_ctx_destroy(ctx: *mut SofiaCtx);
    pub fn sofia_register(
        ctx: *mut SofiaCtx,
        server: *const c_char,
        port: c_int,
        user: *const c_char,
        password: *const c_char,
        display_name: *const c_char,
    );
    pub fn sofia_unregister(ctx: *mut SofiaCtx);
    pub fn sofia_call(ctx: *mut SofiaCtx, number: *const c_char);
    pub fn sofia_answer(ctx: *mut SofiaCtx);
    pub fn sofia_hangup(ctx: *mut SofiaCtx);
    pub fn sofia_set_hold(ctx: *mut SofiaCtx, hold: c_int);
    pub fn sofia_send_dtmf(ctx: *mut SofiaCtx, digit: c_char);
    pub fn sofia_blind_transfer(ctx: *mut SofiaCtx, number: *const c_char);
    pub fn sofia_start_consultation(ctx: *mut SofiaCtx, number: *const c_char);
    pub fn sofia_complete_transfer(ctx: *mut SofiaCtx);
    pub fn sofia_cancel_consultation(ctx: *mut SofiaCtx);
}
