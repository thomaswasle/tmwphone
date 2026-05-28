mod ffi;

use async_channel::{Receiver, Sender};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fmt;

// ── Public event type ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum SipEvent {
    Registered,
    RegistrationFailed(String),
    IncomingCall { from: String },
    CallConnected,
    /// Fired after CallConnected once SDP has been negotiated.
    /// The app should start the RTP audio session with these parameters.
    CallMedia { local_rtp_port: u16, remote_ip: String, remote_rtp_port: u16, codec: u8 },
    CallEnded,
    CallFailed(String),
}

// ── Config passed to register() ───────────────────────────────────────────────

pub struct SipConfig {
    pub server: String,
    pub username: String,
    pub password: String,
    pub display_name: String,
    pub port: u16,
}

// ── Engine ────────────────────────────────────────────────────────────────────

pub struct SipEngine {
    ctx: *mut ffi::SofiaCtx,
    // Keeps the Box<Sender> alive; its raw ptr is held by the C callback.
    _cb_tx: *mut Sender<SipEvent>,
}

impl fmt::Debug for SipEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SipEngine").finish_non_exhaustive()
    }
}

impl Drop for SipEngine {
    fn drop(&mut self) {
        unsafe {
            if !self.ctx.is_null() {
                ffi::sofia_ctx_destroy(self.ctx);
            }
            if !self._cb_tx.is_null() {
                drop(Box::from_raw(self._cb_tx));
            }
        }
    }
}

// Raw pointers are not Send by default; SipEngine must stay on the GTK main
// thread, which is the only thread where GLib main context runs anyway.
// Explicitly not Send.

impl SipEngine {
    pub fn new(server: &str, port: u16) -> (Self, Receiver<SipEvent>) {
        let (tx, rx) = async_channel::unbounded::<SipEvent>();
        let cb_tx: *mut Sender<SipEvent> = Box::into_raw(Box::new(tx));

        let server_c = CString::new(server).unwrap_or_default();
        let ctx = unsafe {
            ffi::sofia_ctx_create(
                server_c.as_ptr(),
                port as c_int,
                sofia_event_cb,
                cb_tx as *mut c_void,
            )
        };
        if ctx.is_null() {
            log::error!("sofia_ctx_create failed — SIP stack could not initialize");
            unsafe { drop(Box::from_raw(cb_tx)) };
            let (tx2, rx2) = async_channel::unbounded::<SipEvent>();
            tx2.try_send(SipEvent::RegistrationFailed(
                "SIP stack failed to start".into(),
            ))
            .ok();
            return (SipEngine { ctx: std::ptr::null_mut(), _cb_tx: std::ptr::null_mut() }, rx2);
        }

        (SipEngine { ctx, _cb_tx: cb_tx }, rx)
    }

    pub fn register(&self, config: SipConfig) {
        let server = CString::new(config.server).unwrap();
        let user = CString::new(config.username).unwrap();
        let password = CString::new(config.password).unwrap();
        let display_name = CString::new(config.display_name).unwrap();
        unsafe {
            ffi::sofia_register(
                self.ctx,
                server.as_ptr(),
                config.port as c_int,
                user.as_ptr(),
                password.as_ptr(),
                display_name.as_ptr(),
            );
        }
    }

    pub fn unregister(&self) {
        unsafe { ffi::sofia_unregister(self.ctx) }
    }

    pub fn make_call(&self, number: &str) {
        let s = CString::new(number).unwrap();
        unsafe { ffi::sofia_call(self.ctx, s.as_ptr()) }
    }

    pub fn answer_call(&self) {
        unsafe { ffi::sofia_answer(self.ctx) }
    }

    pub fn hangup(&self) {
        unsafe { ffi::sofia_hangup(self.ctx) }
    }

    pub fn set_muted(&self, muted: bool) {
        log::debug!("set_muted({muted}) — audio not yet implemented");
    }

    pub fn set_hold(&self, hold: bool) {
        log::debug!("set_hold({hold}) — re-INVITE not yet implemented");
    }

    pub fn send_dtmf(&self, digit: char) {
        if self.ctx.is_null() { return; }
        let c = digit as u8;
        if matches!(c, b'0'..=b'9' | b'*' | b'#') {
            unsafe { ffi::sofia_send_dtmf(self.ctx, c as std::ffi::c_char) }
        }
    }
}

// ── C callback (fires on the GLib main thread) ────────────────────────────────

unsafe extern "C" fn sofia_event_cb(
    event: c_int,
    status: c_int,
    phrase: *const c_char,
    aux: *const c_char,
    userdata: *mut c_void,
) {
    let tx: &Sender<SipEvent> = &*(userdata as *const Sender<SipEvent>);

    let phrase_str = || {
        if phrase.is_null() {
            String::new()
        } else {
            CStr::from_ptr(phrase).to_string_lossy().into_owned()
        }
    };

    let ev = match event {
        ffi::SOFIA_EV_REGISTER_OK => Some(SipEvent::Registered),
        ffi::SOFIA_EV_REGISTER_FAIL => Some(SipEvent::RegistrationFailed(format!(
            "{status} {}", phrase_str()
        ))),
        ffi::SOFIA_EV_INCOMING_CALL => {
            let from = if aux.is_null() {
                "Unknown".to_string()
            } else {
                CStr::from_ptr(aux).to_string_lossy().into_owned()
            };
            Some(SipEvent::IncomingCall { from })
        }
        ffi::SOFIA_EV_CALL_CONNECTED => Some(SipEvent::CallConnected),
        ffi::SOFIA_EV_CALL_MEDIA => {
            // aux = "local_port,remote_ip,remote_port,payload_type"
            let s = if aux.is_null() {
                String::new()
            } else {
                CStr::from_ptr(aux).to_string_lossy().into_owned()
            };
            let parts: Vec<&str> = s.splitn(4, ',').collect();
            if parts.len() >= 3 {
                let local_rtp_port = parts[0].parse::<u16>().unwrap_or(0);
                let remote_ip = parts[1].to_string();
                let remote_rtp_port = parts[2].parse::<u16>().unwrap_or(0);
                let codec = if parts.len() >= 4 { parts[3].parse::<u8>().unwrap_or(0) } else { 0 };
                Some(SipEvent::CallMedia { local_rtp_port, remote_ip, remote_rtp_port, codec })
            } else {
                None
            }
        }
        ffi::SOFIA_EV_CALL_ENDED => Some(SipEvent::CallEnded),
        ffi::SOFIA_EV_CALL_FAILED => Some(SipEvent::CallFailed(format!(
            "{status} {}", phrase_str()
        ))),
        _ => None,
    };

    if let Some(ev) = ev {
        log::debug!("SIP event: {ev:?}");
        tx.try_send(ev).ok();
    }
}
